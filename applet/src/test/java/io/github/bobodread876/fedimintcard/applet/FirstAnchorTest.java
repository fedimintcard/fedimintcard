package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.ClockTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Map;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * The anchor, adapted from Cashu card 3.1's FirstAnchorTest (D204). While no PIN is set and no slot holds value, a header
 * that is not weak is an anchor: in one transaction the clock goes to its time, even an earlier one, the hardest is raised
 * to its bits only where harder (an anchor never lowers it), and the day's window and spent count are cleared. It marks
 * this selection as anchored, and SET_PIN asks for that, so an anchor sent in an earlier selection, a stranger's
 * included, never counts. Unlike Cashu's, every header on such a card is an anchor, not only the first set-up's, and a
 * weak one is refused (6A93) rather than taken.
 *
 * <p>REFLECTION: the tests' floor, as in WeakClockTest. GROWN_BITS (twice CLOCK_BITS's work) is the network grown since
 * the floor was set; CLOCK_BITS is the cheap far header, just over twice the floor's work.
 */
class FirstAnchorTest {

    @BeforeEach
    void theTestFloor() { Headers.useTheTestFloor(); }

    @AfterEach
    void theBuiltFloor() { Headers.useTheBuiltFloor(); }

    @Test
    void aStrangersAnchorNeverCountsAndTheSetUpsTipBringsTheClockBack() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        assertEquals(0x9000, header(c, net(F)), "a stranger, no PIN, a fresh card: the cheap far header");
        assertEquals(F, now(c));
        assertArrayEquals(Headers.le32(Headers.CLOCK_BITS), hardest(c), "its bits the hardest");
        assertEquals(1, anchored(c), "the stranger's selection anchored");
        c.select();
        assertEquals(0, anchored(c), "the next has not");
        Map<String, Object> before = c.persistent();
        assertSw(0x6A96, c.tx(SET_PIN, 0, DEFAULT_PIN), "so SET_PIN is refused");
        assertEquals(before, c.persistent(), "and writes nothing");
        setUp(c, grown(T0));
        assertEquals(T0, now(c), "the set-up's tip, older, is the clock");
        assertArrayEquals(Headers.le32(GROWN_BITS), hardest(c), "and the hardest");
        assertEquals(1, c.pinState());
    }

    @Test
    void anAnchorNeverLowersTheHardest() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertEquals(0x9000, header(c, grown(F)), "a far header of the network's grown work");
        setUp(c, Headers.mine(T0, 0x203FFFFEL));
        assertEquals(T0, now(c), "the anchor less hard than the hardest brings the clock back");
        assertArrayEquals(Headers.le32(GROWN_BITS), hardest(c), "and the hardest stays");
        CardTestSupport d = new CardTestSupport();
        assertEquals(0x9000, header(d, grown(F)));
        d.select();
        assertEquals(0x6A93, header(d, net(T0)), "an anchor weak beside the hardest is refused");
        assertEquals(F, now(d));
        assertEquals(0, anchored(d));
    }

    @Test
    void theAnchorClearsTheDaysWindowAndItsCountAndKeepsTheLimit() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_LIMIT, 0, OfflineTest.longBytes(5000)));
        // REFLECTION: no command leaves a window or a count on a card with no PIN; the anchor clears them all the same
        byte[] clock = (byte[]) c.field("clock");
        java.util.Arrays.fill(clock, 4, 8, (byte) 0x11);
        java.util.Arrays.fill(clock, 20, 28, (byte) 0x22);
        assertEquals(0x9000, header(c, net(T0)));
        assertEquals(T0, now(c));
        assertEquals(0, window(c), "the window cleared");
        assertEquals(0, spent(c), "and its count");
        assertEquals(5000, OfflineTest.u64(state(c), 40), "the limit kept");
    }

    @Test
    void afterTheCheapFarHeaderInTransitTheSetUpsTipIsTheBarAndTheCardPaysADayAtATime() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_LIMIT, 0, OfflineTest.longBytes(1000)));
        assertEquals(0x9000, header(c, net(F)), "in transit: the cheap far header");
        setUp(c, grown(T0));
        notes(c, 3, 1000);
        assertEquals(T0, now(c));
        assertEquals(0x9000, pay(c, 0), "day 1");
        c.select();
        assertEquals(0x6A93, header(c, net(F)), "the stranger again, the same header: weak now");
        assertEquals(T0, now(c));
        assertEquals(0x6A95, pay(c, 1));
        assertEquals(0x9000, header(c, grown(T0 + DAY + 120)));
        assertEquals(0x9000, pay(c, 1), "day 2");
        assertEquals(0x9000, header(c, grown(T0 + 2 * DAY + 180)));
        assertEquals(0x9000, pay(c, 2), "day 3: the last note");
    }

    @Test
    void whatIsLeftAFarHeaderWithHalfTheTipsWorkIsTaken() throws Exception {
        CardTestSupport c = card(0, grown(T0), 0, 0);
        assertEquals(0x9000, header(c, Headers.mine(F, 0x203FFFFEL)), "half the tip's work, far: not weak (accepted)");
        assertEquals(F, now(c));
        CardTestSupport d = card(0, grown(T0), 0, 0);
        assertEquals(0x6A93, header(d, Headers.mine(F, 0x203FFFFFL)), "a hair under half");
        assertEquals(T0, now(d));
    }

    @Test
    void onlyWhileNoPinIsSetAndNoSlotHoldsValue() throws Exception {
        // after the set-up
        CardTestSupport c = card(0, grown(T0 + HOUR), 0, 0);
        c.select();
        assertEquals(0x9000, header(c, grown(T0)), "an older header after the set-up");
        assertEquals(T0 + HOUR, now(c), "moves nothing");
        assertEquals(0, anchored(c));
        // a card with no PIN that holds a note, or is owed change
        for (int st : new int[] { ST_NOTE, ST_CHANGE }) {
            CardTestSupport d = new CardTestSupport();
            assertSw(0x9000, d.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
            assertEquals(0x9000, header(d, net(F)));
            d.genKey(0);
            assertSw(0x9000, d.tx(SET_NOTE, 0, d.note(0, 1000)));
            ((byte[]) d.field("state"))[0] = (byte) st;   // REFLECTION for ST_CHANGE: change owed to the slot
            d.select();
            assertEquals(0x9000, header(d, grown(T0)), "the tip, state " + st);
            assertEquals(F, now(d), "is no anchor: the card holds value, state " + st);
            assertEquals(0, anchored(d));
            assertSw(0x6A96, d.tx(SET_PIN, 0, DEFAULT_PIN), "state " + st);
            assertEquals(0, d.pinState());
        }
    }

    @Test
    void aWeakAnchorIsRefusedAndSetPinWithIt() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        Map<String, Object> before = c.persistent();
        assertEquals(0x6A93, header(c, floorAt(T0)), "a tip at the floor's work");
        assertEquals(0, anchored(c));
        assertSw(0x6A96, c.tx(SET_PIN, 0, DEFAULT_PIN));
        assertEquals(before, c.persistent(), "nothing written");
    }
}
