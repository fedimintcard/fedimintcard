package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import io.github.bobodread876.fedimintcard.applet.Bip340;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * Review finding 2: one header far ahead, at no more than the floor's work and sent by a stranger without the PIN,
 * moves the clock there; the day window then begins at that time and never ends, so the daily limit becomes a
 * lifetime limit. The fix must make these tests fail.
 *
 * <p>REFLECTION: the applet's FLOOR_BITS is lowered to the tests' floor (0x207FFFFF, half of all hashes), as Cashu
 * card 3.1's FarHeaderTest does, so the far header can be mined here. Every other check of SET_HEADER runs as built.
 * On a chip the same header needs the built floor (0x17087BC0): about a quarter of block 900 000's work.
 */
class Proof2HeaderClockTest {
    static final byte[] BUILT_FLOOR = floorNow();

    /** Real mainnet blocks 900 000 and 900 200 (from OfflineTest), June 2025, bits 0x17023774. */
    static final byte[] H900000 = Bip340.hex("00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92");
    static final byte[] H900200 = Bip340.hex("0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104");

    static final long LIMIT = 10_000_000L;     // 10 000 sat a day
    static final long NOTE = 6_000_000L;       // so two notes fit no day
    static final long T0 = 1_800_000_000L;     // 2027
    static final long DAY = 86_400L;
    /** So far ahead that a day past it does not fit four bytes. */
    static final long TOP = 0xFFFFFF00L;

    @BeforeEach
    void theTestFloor() { useFloor(le32(TEST_BITS)); }

    @AfterEach
    void theBuiltFloor() { useFloor(BUILT_FLOOR); }

    /** A provisioned card: limit, four notes of NOTE each, PIN. */
    static SimCard card() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(SET_LIMIT, 0, be64(LIMIT)));
        for (int s = 0; s < 4; s++) {
            assertEquals(0x9000, c.sw(GEN_KEY, s, null));
            assertEquals(0x9000, c.sw(SET_NOTE, s, noteData(NOTE)));
        }
        assertEquals(0x9000, c.sw(SET_PIN, 0, PIN));
        return c;
    }

    static int spend(SimCard c, int slot) {
        c.powerCycle();
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        return c.sw(SIGN, slot, SimCard.sha256(new byte[] { (byte) slot }));
    }

    /** A card in ordinary use, then one stranger's far header: the day that follows never ends. */
    @Test
    void aFarHeaderAfterOrdinaryUseMakesTheLimitForLife() throws Exception {
        SimCard c = card();
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(T0, CLOCK_BITS)), "the holder's ordinary clock");
        assertEquals(0x9000, spend(c, 0));
        assertEquals(T0, c.window());
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(T0 + DAY + 60, CLOCK_BITS)));
        assertEquals(0x9000, spend(c, 1), "a new day: ordinary");
        assertEquals(T0 + DAY + 60, c.window());

        c.powerCycle();   // the stranger: no PIN, a floor-work header at the top of the clock
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(TOP, TEST_BITS)));
        assertEquals(TOP, c.now());

        assertEquals(0x9000, spend(c, 2), "the next spend begins a day at TOP");
        assertEquals(TOP, c.window());
        for (long t : new long[] { T0 + 2 * DAY, T0 + 400 * DAY, 0xFFFFFFFFL }) {
            assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(t, CLOCK_BITS)));
            assertEquals(0x6A95, spend(c, 3), "over the limit, header at " + t);
        }
        assertEquals(0xFFFFFFFFL, c.now(), "the largest time there is, and the day still has not ended");
        assertEquals(TOP, c.window());
        assertEquals(NOTE, c.spentToday());
    }

    /** A fresh card whose first header is the stranger's: real mainnet headers afterwards change nothing. */
    @Test
    void realHeadersAfterAFarOneNeverMoveTheClock() throws Exception {
        SimCard c = card();
        c.powerCycle();
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(TOP, TEST_BITS)), "the stranger's header first");
        assertEquals(0x9000, c.sw(SET_HEADER, 0, H900000), "a real block is taken ...");
        assertEquals(0x9000, c.sw(SET_HEADER, 0, H900200));
        assertEquals(TOP, c.now(), "... but the clock stays at TOP");
        assertEquals(0x9000, spend(c, 0));
        assertEquals(0x6A95, spend(c, 1), "the second note of the card's life is over the limit");
        byte[] bits = (byte[]) c.field("clock");
        assertArrayEquals(le32(TEST_BITS), java.util.Arrays.copyOfRange(bits, 8, 12),
            "the hardest header is still the stranger's: a later header raises it only when it is also later in time");
    }
}
