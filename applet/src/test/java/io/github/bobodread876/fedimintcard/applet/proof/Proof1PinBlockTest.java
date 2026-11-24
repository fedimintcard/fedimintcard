package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/**
 * Review finding 1: three wrong VERIFY_PINs, sent by any reader with no prior right, block the PIN for good, and no
 * instruction brings it back, so every note on the card can never be signed again. The fix must make these tests fail.
 */
class Proof1PinBlockTest {

    /** A card holding a note, then a stranger's three wrong tries. */
    static SimCard blockedCardWithANote() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(GEN_KEY, 0, null));
        assertEquals(0x9000, c.sw(SET_NOTE, 0, noteData(50_000_000L)));
        assertEquals(0x9000, c.sw(SET_PIN, 0, PIN));
        c.powerCycle();   // the stranger's tap: a new session, no PIN given
        assertEquals(0x63C2, c.sw(VERIFY_PIN, 0, WRONG));
        assertEquals(0x63C1, c.sw(VERIFY_PIN, 0, WRONG));
        assertEquals(0x6983, c.sw(VERIFY_PIN, 0, WRONG), "the third wrong try blocks");
        assertEquals(2, c.pinState(), "blocked is written to persistent state");
        return c;
    }

    @Test
    void threeWrongTriesBlockAcrossDeselectReselectAndPower() throws Exception {
        SimCard c = blockedCardWithANote();
        assertEquals(0x6983, c.sw(VERIFY_PIN, 0, PIN), "the right PIN, same session");
        assertEquals(0x9000, c.select().getSW(), "re-SELECT");
        assertEquals(0x6983, c.sw(VERIFY_PIN, 0, PIN), "the right PIN after re-SELECT");
        // SELECT of an AID that is not on the card, then back: the applet is deselected in between.
        c.sim.transmitCommand(new javax.smartcardio.CommandAPDU(0x00, 0xA4, 0x04, 0x00,
            io.github.bobodread876.fedimintcard.applet.Bip340.hex("A000000151000000")));
        assertEquals(0x9000, c.select().getSW());
        assertEquals(0x6983, c.sw(VERIFY_PIN, 0, PIN), "the right PIN after another SELECT and back");
        for (int i = 0; i < 5; i++) {
            c.powerCycle();
            assertEquals(0x6983, c.sw(VERIFY_PIN, 0, PIN), "the right PIN after power cycle " + i);
        }
        assertEquals(0x6983, c.sw(SIGN, 0, new byte[32]), "the note can never be signed");
        assertEquals(0x6983, c.sw(SET_PIN, 0, PIN));
        byte[] change = new byte[8];
        System.arraycopy(PIN, 0, change, 0, 4);
        System.arraycopy(PIN, 0, change, 4, 4);
        assertEquals(0x6983, c.sw(CHANGE_PIN, 0, change));
        assertEquals(ST_NOTE, c.slotState(0), "the value is still on the card, out of reach");
    }

    /** Every INS under CLA B0 (and the ISO PIN instructions under CLA 00 and 80), with no data and with the right PIN. */
    @Test
    void noInstructionBringsItBack() throws Exception {
        SimCard c = blockedCardWithANote();
        int[] p1s = { 0, 1, 2, 0x80 };
        for (int ins = 0; ins < 256; ins++) {
            if (ins == 0xA4 || (ins & 0xF0) == 0x60 || (ins & 0xF0) == 0x90) continue;   // SELECT and invalid INS
            for (int p1 : p1s) {
                c.tx(CLA, ins, p1, 0, null);
                c.tx(CLA, ins, p1, 0, PIN);
                c.tx(CLA, ins, p1, 1, PIN);
            }
        }
        for (int cla : new int[] { 0x00, 0x80 }) {
            for (int ins : new int[] { 0x20, 0x24, 0x2C, 0x44 }) {
                c.tx(cla, ins, 0, 0, null);
                c.tx(cla, ins, 0, 0, PIN);
            }
        }
        c.powerCycle();
        assertEquals(2, c.pinState(), "still blocked");
        assertEquals(0x6983, c.sw(VERIFY_PIN, 0, PIN));
        assertEquals(0x6983, c.sw(SIGN, 0, new byte[32]));
    }

    /**
     * Noticed on the way: a blocked card still takes new value. GEN_KEY on an empty slot and SET_NOTE need no PIN, and
     * GET_STATE does not report the PIN's state, so a till that does not try VERIFY_PIN first loads money it can never
     * spend.
     */
    @Test
    void aBlockedCardStillTakesNewNotes() throws Exception {
        SimCard c = blockedCardWithANote();
        assertEquals(0x9000, c.sw(GEN_KEY, 1, null));
        assertEquals(0x9000, c.sw(SET_NOTE, 1, noteData(20_000_000L)));
        assertEquals(ST_NOTE, c.slotState(1));
        assertEquals(59, c.state().length, "GET_STATE: no PIN state byte among its 59");
        assertEquals(0x6983, c.sw(SIGN, 1, new byte[32]));
    }
}
