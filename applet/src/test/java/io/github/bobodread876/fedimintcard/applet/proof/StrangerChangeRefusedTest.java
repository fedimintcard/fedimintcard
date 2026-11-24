package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/**
 * After SET_PIN, SET_CHANGE needs this selection's PIN (DESIGN § 2, I8): a reader with no PIN cannot turn the free
 * slots into change, which GEN_KEY never re-keys.
 */
class StrangerChangeRefusedTest {
    static final int ST_CHANGE = 3, SET_CHANGE = 0x17;

    static byte[] change(long msat) {
        byte[] d = new byte[74];
        System.arraycopy(be64(msat), 0, d, 0, 8);
        for (int i = 8; i < 74; i++) d[i] = (byte) i;
        return d;
    }

    @Test
    void aStrangerCannotTurnAFreeSlotIntoChange() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(GEN_KEY, 0, null), "the till's spare");
        assertEquals(0x9000, c.setPin());
        c.powerCycle();   // a stranger, no PIN
        for (int s = 0; s < 32; s++) {
            if (s != 0) assertEquals(0x9000, c.sw(GEN_KEY, s, null), "slot " + s);
            byte[] before = c.tx(GET_SLOT, s, null).getData();
            assertEquals(0x6A94, c.sw(SET_CHANGE, s, change(1)), "slot " + s);
            assertEquals(ST_KEYED, c.slotState(s));
            assertArrayEquals(before, c.tx(GET_SLOT, s, null).getData(), "nothing written");
        }
        c.powerCycle();   // the holder's offline POS, with the PIN: change still lands, and spares re-key
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        assertEquals(0x9000, c.sw(SET_CHANGE, 0, change(4096)));
        assertEquals(ST_CHANGE, c.slotState(0));
        for (int s = 1; s < 32; s++) {
            assertEquals(0x9000, c.tx(CLA, GEN_KEY, s, 1, null).getSW(), "slot " + s + " is re-keyed");
        }
    }

    @Test
    void beforeSetPinChangeNeedsNoPin() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(GEN_KEY, 0, null));
        assertEquals(0x9000, c.sw(SET_CHANGE, 0, change(4096)));
        assertEquals(ST_CHANGE, c.slotState(0));
    }
}
