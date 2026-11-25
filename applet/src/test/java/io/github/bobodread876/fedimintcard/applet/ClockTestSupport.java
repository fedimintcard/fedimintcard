package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.Arrays;

/** What the clock's tests read and send (WeakClockTest, FirstAnchorTest, FarHeaderTest). Headers at the tests' floor. */
final class ClockTestSupport {
    private ClockTestSupport() {}

    static final long T0 = 1_800_000_000L;     // 2027
    static final long F = 3_000_000_000L;      // far ahead; a day past it still fits four bytes (2065)
    static final long TOP = 0xFFFFFF00L;       // so far ahead that a day past it does not fit four bytes
    static final long DAY = 86_400L, HOUR = 3_600L;
    /** Twice the work of CLOCK_BITS: the network grown. */
    static final long GROWN_BITS = 0x201FFFFFL;
    /** A target of exactly twice CLOCK_BITS's: half its work, the least that is not weak beside it. */
    static final long HALF_BITS = 0x207FFFFEL;

    static byte[] net(long t) { return Headers.mine(t, Headers.CLOCK_BITS); }
    static byte[] grown(long t) { return Headers.mine(t, GROWN_BITS); }
    static byte[] floorAt(long t) { return Headers.mine(t, Headers.TEST_BITS); }

    static int header(CardTestSupport c, byte[] h) { return c.tx(SET_HEADER, 0, h).getSW(); }
    static byte[] state(CardTestSupport c) { return c.tx(GET_STATE, 0, null).getData(); }
    static long now(CardTestSupport c) { return OfflineTest.u32(state(c), 32); }
    static long window(CardTestSupport c) { return OfflineTest.u32(state(c), 36); }
    static long spent(CardTestSupport c) { return OfflineTest.u64(state(c), 48); }
    static int anchored(CardTestSupport c) { return state(c)[63]; }
    /** The hardest bits the card has taken, as a header carries them (test-only reflection). */
    static byte[] hardest(CardTestSupport c) throws ReflectiveOperationException {
        return Arrays.copyOfRange((byte[]) c.field("clock"), 8, 12);
    }

    /** A set-up as the Setup app sends it, in one selection: the registry key, the anchor, SET_PIN. */
    static void setUp(CardTestSupport c, byte[] tip) {
        c.select();
        c.certify();
        if (!registryKeySet(c)) assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        assertEquals(0x9000, header(c, tip), "the set-up's anchor");
        assertEquals(1, anchored(c));
        assertSw(0x9000, c.tx(SET_PIN, 0, DEFAULT_PIN));
    }

    static boolean registryKeySet(CardTestSupport c) {
        try {
            return (Boolean) c.field("registryKeySet");
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    /** Notes of `msat` in slots 0..n-1, in the set-up's selection (it has the PIN). */
    static void notes(CardTestSupport c, int n, long msat) {
        for (int s = 0; s < n; s++) {
            c.genKey(s);
            assertSw(0x9000, c.tx(SET_NOTE, s, c.note(s, msat)));
        }
    }

    /** A card with a day's limit, set up at `tip`, and `n` notes of `msat`. */
    static CardTestSupport card(long limit, byte[] tip, int n, long msat) {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_LIMIT, 0, OfflineTest.longBytes(limit)));
        setUp(c, tip);
        notes(c, n, msat);
        return c;
    }

    /** A payment tap: a new selection, the PIN, SIGN of slot `s`. */
    static int pay(CardTestSupport c, int s) {
        c.select();
        c.verifyPin(DEFAULT_PIN);
        return c.tx(SIGN, s, Headers.sha256(new byte[] { (byte) s })).getSW();
    }
}
