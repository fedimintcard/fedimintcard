package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import org.junit.jupiter.api.Test;

/**
 * Review finding 4: SET_NOTE takes any reader's note into any KEYED slot, with no PIN and no tie to the till that keyed
 * it, and the amount it carries is what the daily limit counts. The fix must make these tests fail.
 */
class Proof4SetNoteTest {
    static final long LIMIT = 10_000_000L;

    static SimCard keyedCard() {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(SET_LIMIT, 0, be64(LIMIT)));
        assertEquals(0x9000, c.sw(GEN_KEY, 3, null), "the till keys slot 3 and asks the federation for its note");
        assertEquals(0x9000, c.sw(SET_PIN, 0, PIN));
        return c;
    }

    @Test
    void aStrangerWritesTheAmountTheLimitCounts() throws Exception {
        SimCard c = keyedCard();
        c.powerCycle();   // a stranger's tap: new session, no PIN, never saw this slot
        assertEquals(0x9000, c.sw(SET_NOTE, 3, noteData(1L)), "1 msat, the writer's word");
        assertEquals(ST_NOTE, c.slotState(3));
        byte[] slot = c.tx(GET_SLOT, 3, null).getData();
        assertEquals(1L, SimCard.u(slot, 33, 8), "the card believes the note is 1 msat");
        assertArrayEquals(Arrays.copyOfRange(noteData(1L), 8, 56), Arrays.copyOfRange(slot, 41, 89),
            "and keeps the writer's 48 bytes as the federation's signature");
        assertEquals(0x6985, c.sw(SET_NOTE, 3, noteData(1_000_000_000L)), "the till's real note is now refused");

        c.powerCycle();   // someone with the PIN spends it
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        assertEquals(0x9000, c.sw(SIGN, 3, SimCard.sha256(new byte[] { 3 })));
        assertEquals(1L, c.spentToday(), "the day's count took 1 msat, whatever the key's real note is worth");

        // The same key holding its real 1 000 000 sat note, written by the till: refused over the limit.
        SimCard honest = keyedCard();
        assertEquals(0x9000, honest.sw(SET_NOTE, 3, noteData(1_000_000_000L)));
        assertEquals(0x9000, honest.sw(VERIFY_PIN, 0, PIN));
        assertEquals(0x6A95, honest.sw(SIGN, 3, SimCard.sha256(new byte[] { 3 })));
    }

    /**
     * Noticed on the way, and worse than the review says: on a card with a daily limit, one stranger's tap with no PIN
     * kills every free slot for good. GEN_KEY on an EMPTY slot needs no PIN; SET_NOTE then writes an amount over the
     * limit; SIGN refuses it (6A95) for ever, and GEN_KEY never re-keys a NOTE slot, so the card can take no more value.
     */
    @Test
    void aStrangerFillsEveryFreeSlotForGood() throws Exception {
        SimCard c = keyedCard();
        assertEquals(0x9000, c.sw(SET_NOTE, 3, noteData(5_000_000L)), "the holder's one real note");
        c.powerCycle();   // the stranger
        for (int s = 0; s < 32; s++) {
            if (s == 3) continue;
            assertEquals(0x9000, c.sw(GEN_KEY, s, null), "slot " + s);
            assertEquals(0x9000, c.sw(SET_NOTE, s, noteData(-1L)), "slot " + s);
        }
        c.powerCycle();   // the holder, with the PIN
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        for (int s = 0; s < 32; s++) {
            if (s == 3) continue;
            assertEquals(0x6A95, c.sw(SIGN, s, SimCard.sha256(new byte[] { (byte) s })), "slot " + s + " cannot be spent");
            assertEquals(0x6985, c.tx(CLA, GEN_KEY, s, 1, null).getSW(), "slot " + s + " cannot be re-keyed");
        }
        for (int s = 0; s < 32; s++) assertNotEquals(ST_EMPTY, c.slotState(s), "no slot left for a top-up");
        assertEquals(0x9000, c.sw(SIGN, 3, SimCard.sha256(new byte[] { 3 })), "the real note still spends");
    }
}
