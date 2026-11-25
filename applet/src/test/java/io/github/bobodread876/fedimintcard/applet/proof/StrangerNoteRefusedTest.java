package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import io.github.bobodread876.fedimintcard.applet.Registry;
import java.math.BigInteger;
import java.security.PrivateKey;
import java.util.Arrays;
import org.junit.jupiter.api.Test;

/**
 * A note reaches a slot only with the registry's signature over that slot's key (DESIGN § 6, I8). A reader with no
 * registry signature cannot write a note, so it cannot set the amount the day's limit counts, nor fill the free slots.
 */
class StrangerNoteRefusedTest {
    static final long LIMIT = 10_000_000L;
    /** A P-256 key that is not the registry's. */
    static final PrivateKey STRANGER = Registry.privateKey(BigInteger.TWO);

    static SimCard keyedCard() {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(SET_LIMIT, 0, be64(LIMIT)));
        assertEquals(0x9000, c.sw(GEN_KEY, 3, null), "the till keys slot 3 and asks the federation for its note");
        assertEquals(0x9000, c.setPin());
        return c;
    }

    @Test
    void aStrangersNoteIsRefused() throws Exception {
        SimCard c = keyedCard();
        byte[] before = c.tx(GET_SLOT, 3, null).getData();
        c.powerCycle();   // a stranger's tap: new session, no PIN
        byte[] x = c.slotX(3);
        assertEquals(0x6700, c.sw(SET_NOTE, 3, noteData(1L)), "there is no unsigned form");
        assertEquals(0x6982, c.sw(SET_NOTE, 3, Registry.signed(STRANGER, x, noteData(1L))), "the stranger's own signature");
        byte[] real = c.note(3, 1_000_000_000L);
        byte[] lowered = real.clone();
        System.arraycopy(be64(1L), 0, lowered, 0, 8);
        assertEquals(0x6982, c.sw(SET_NOTE, 3, lowered), "the registry's signature over another amount");
        assertEquals(ST_KEYED, c.slotState(3));
        assertArrayEquals(before, c.tx(GET_SLOT, 3, null).getData(), "nothing written");

        assertEquals(0x9000, c.sw(SET_NOTE, 3, real), "the till's real note still loads");
        byte[] slot = c.tx(GET_SLOT, 3, null).getData();
        assertArrayEquals(Arrays.copyOf(real, 56), Arrays.copyOfRange(slot, 33, 89), "the registry's amount and signature");

        c.powerCycle();   // the day's limit counts the registry's amount
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        assertEquals(0x6A95, c.sw(SIGN, 3, SimCard.sha256(new byte[] { 3 })));
        assertEquals(0L, c.spentToday());
    }

    @Test
    void aStrangerCannotFillTheFreeSlots() throws Exception {
        SimCard c = keyedCard();
        assertEquals(0x9000, c.sw(SET_NOTE, 3, c.note(3, 5_000_000L)), "the holder's one real note");
        c.powerCycle();   // the stranger
        for (int s = 0; s < 32; s++) {
            if (s == 3) continue;
            assertEquals(0x9000, c.sw(GEN_KEY, s, null), "slot " + s);
            assertEquals(0x6700, c.sw(SET_NOTE, s, noteData(-1L)), "slot " + s);
            assertEquals(0x6982, c.sw(SET_NOTE, s, Registry.signed(STRANGER, c.slotX(s), noteData(-1L))), "slot " + s);
            assertEquals(ST_KEYED, c.slotState(s), "slot " + s);
        }
        c.powerCycle();   // the holder's till, with the PIN, re-keys what it did not key and loads a top-up
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        for (int s = 0; s < 32; s++) {
            if (s == 3) continue;
            if (s == 0) {
                // Slot 0 is the key the certificate names (this fixture's stand-in certificate is set
                // with the PIN): the re-key form never takes it after SET_CERT.
                assertEquals(0x6985, c.tx(CLA, GEN_KEY, 0, 1, null).getSW(), "slot 0 is the certificate's");
                continue;
            }
            assertEquals(0x9000, c.tx(CLA, GEN_KEY, s, 1, null).getSW(), "slot " + s + " is re-keyed");
        }
        assertEquals(0x9000, c.sw(SET_NOTE, 1, c.note(1, 2_000_000L)), "a top-up loads");
        assertEquals(0x9000, c.sw(SIGN, 3, SimCard.sha256(new byte[] { 3 })), "the real note still spends");
        assertEquals(0x9000, c.sw(SIGN, 1, SimCard.sha256(new byte[] { 1 })), "and so does the top-up");
    }
}
