package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.List;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * Wrong PINs never block the card: a stranger's wrong tries leave a wait, never a dead card. No command answers 6983,
 * the right PIN works once its wait is served, across SELECTs and power, and a card that owes a wait says so to a till.
 */
class PinNeverBlocksTest {

    static final int KEEP_HOLDING = 0x6A98;

    /** A card holding a note, then a stranger's three wrong tries in a new session. */
    static SimCard cardAfterThreeWrongTries() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(GEN_KEY, 0, null));
        assertEquals(0x9000, c.setPin());
        assertEquals(0x9000, c.sw(SET_NOTE, 0, c.note(0, 50_000_000L)));
        c.powerCycle();   // the stranger's tap: a new session, no PIN given
        assertEquals(0x63C1, c.sw(VERIFY_PIN, 0, WRONG));
        assertEquals(0x63C2, held(c, WRONG));
        assertEquals(0x63C3, held(c, WRONG));
        assertEquals(1, c.pinState(), "the PIN is still set");
        return c;
    }

    /** VERIFY_PIN sent again while it answers 6A98, as a till holding the card does; its last answer. */
    static int held(SimCard c, byte[] pin) {
        int n = 0, sw;
        while ((sw = c.sw(VERIFY_PIN, 0, pin)) == KEEP_HOLDING) assertTrue(++n < 100, "the wait goes down");
        return sw;
    }

    static void theNoteSigns(SimCard c) {
        ResponseAPDU r = c.tx(SIGN, 0, new byte[32]);
        assertEquals(0x9000, r.getSW(), "the note signs");
        assertEquals(64, r.getData().length);
    }

    @Test
    void theRightPinWorksOnceItsWaitIsServedAcrossSelectAndPower() throws Exception {
        SimCard c = cardAfterThreeWrongTries();
        assertEquals(KEEP_HOLDING, c.sw(VERIFY_PIN, 0, PIN), "the right PIN first owes the wait");
        assertEquals(0x9000, c.select().getSW(), "re-SELECT");
        assertEquals(KEEP_HOLDING, c.sw(VERIFY_PIN, 0, PIN));
        // SELECT of an AID that is not on the card, then back: the applet is deselected in between.
        c.sim.transmitCommand(new javax.smartcardio.CommandAPDU(0x00, 0xA4, 0x04, 0x00,
            io.github.bobodread876.fedimintcard.applet.Bip340.hex("A000000151000000")));
        assertEquals(0x9000, c.select().getSW());
        for (int i = 0; i < 5; i++) {
            c.powerCycle();
            assertEquals(KEEP_HOLDING, c.sw(VERIFY_PIN, 0, PIN), "still a wait after power cycle " + i + ", never 6983");
        }
        assertEquals(0x9000, held(c, PIN), "the wait served, the right PIN opens the card");
        theNoteSigns(c);
        assertEquals(0x6985, c.sw(SET_PIN, 0, PIN), "a PIN is set: SET_PIN refuses, and says nothing of a block");
        assertEquals(ST_SPENT, c.slotState(0));
    }

    /** Every INS under CLA B0 (and the ISO PIN instructions under CLA 00 and 80), with no data and with the right PIN. */
    @Test
    void noCommandAnswers6983() throws Exception {
        SimCard c = cardAfterThreeWrongTries();
        List<String> blocked = new ArrayList<>();
        int[] p1s = { 0, 1, 2, 0x80 };
        for (int ins = 0; ins < 256; ins++) {
            if (ins == 0xA4 || (ins & 0xF0) == 0x60 || (ins & 0xF0) == 0x90) continue;   // SELECT and invalid INS
            for (int p1 : p1s) {
                for (ResponseAPDU r : new ResponseAPDU[] { c.tx(CLA, ins, p1, 0, null), c.tx(CLA, ins, p1, 0, PIN), c.tx(CLA, ins, p1, 1, PIN) }) {
                    if (r.getSW() == 0x6983) blocked.add(String.format("B0 %02X %02X", ins, p1));
                }
            }
        }
        for (int cla : new int[] { 0x00, 0x80 }) {
            for (int ins : new int[] { 0x20, 0x24, 0x2C, 0x44 }) {
                if (c.tx(cla, ins, 0, 0, null).getSW() == 0x6983 || c.tx(cla, ins, 0, 0, PIN).getSW() == 0x6983) {
                    blocked.add(String.format("%02X %02X", cla, ins));
                }
            }
        }
        assertEquals(List.of(), blocked, "no command answers 6983");
        c.powerCycle();
        assertEquals(1, c.pinState(), "set, never blocked");
        assertEquals(ST_NOTE, c.slotState(0), "the note is on the card");
        assertEquals(0x9000, held(c, PIN));
        theNoteSigns(c);
    }

    /** GET_STATE bytes 59..62: the PIN's state, the wrong tries in a row and the askings owed, so a till can say how long to hold. */
    @Test
    void aCardThatOwesAWaitShowsItToATill() throws Exception {
        SimCard c = cardAfterThreeWrongTries();
        byte[] st = c.state();
        assertEquals(65, st.length);
        assertEquals(1, st[59], "PIN set");
        assertEquals(3, st[60], "three wrong tries in a row");
        assertEquals(4, u(st, 61, 2), "four askings owed before the next try");
        assertEquals(KEEP_HOLDING, c.sw(VERIFY_PIN, 0, PIN));
        assertEquals(4, u(c.state(), 61, 2), "a stretch not yet over writes nothing");
        assertEquals(0x9000, held(c, PIN));
        st = c.state();
        assertEquals(0, st[60], "the right PIN clears the count");
        assertEquals(0, u(st, 61, 2), "and the wait");
    }
}
