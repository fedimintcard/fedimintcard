package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.ClockTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Map;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * The weak rule, adapted from Cashu card 3.1's WeakClockTest (D201, D203). A header is weak if its target is more than
 * twice the hardest's the card has taken, or, before any hardest, if its work is less than twice the floor's. Here a weak
 * header answers 6A93 and writes nothing: this card has no REFRESH and no weak flag, so a far header at the floor's work
 * never reaches the clock. A header that is not weak moves the clock when later, and raises the hardest only where it is
 * harder.
 *
 * <p>REFLECTION: the applet's FLOOR_BITS is lowered to the tests' floor (Headers.TEST_BITS), as Cashu card 3.1's tests do,
 * so headers can be mined here. The cards are set up at a header of CLOCK_BITS (twice the floor's work), which stands for
 * the network's blocks.
 */
class WeakClockTest {

    @BeforeEach
    void theTestFloor() { Headers.useTheTestFloor(); }

    @AfterEach
    void theBuiltFloor() { Headers.useTheBuiltFloor(); }

    @Test
    void aWeakHeaderAnswers6A93AndWritesNothingFarOrNear() throws Exception {
        CardTestSupport c = card(1000, net(T0), 3, 1000);
        assertEquals(0x9000, pay(c, 0), "day 1: the day's limit spent");
        for (long t : new long[] { F, TOP, 0xFFFFFFFFL, T0 + HOUR, T0 - HOUR }) {
            c.select();   // a stranger, no PIN
            Map<String, Object> before = c.persistent();
            assertEquals(0x6A93, header(c, floorAt(t)), "a header at the floor's work at " + t);
            assertEquals(before, c.persistent(), "nothing written, at " + t);
        }
        assertEquals(T0, now(c));
        assertEquals(T0, window(c));
        assertEquals(0x6A95, pay(c, 1), "no day was given");
        assertEquals(0x9000, header(c, net(T0 + DAY + 60)), "a day on, by the network");
        assertEquals(0x9000, pay(c, 1), "day 2");
    }

    @Test
    void twiceTheHardestsTargetIsNotWeakAndAHarderHeaderRaisesTheBar() throws Exception {
        CardTestSupport c = card(0, net(T0), 0, 0);
        assertArrayEquals(Headers.le32(Headers.CLOCK_BITS), hardest(c));
        assertEquals(0x9000, header(c, Headers.mine(T0 + 60, HALF_BITS)), "a target of exactly twice the hardest's");
        assertEquals(T0 + 60, now(c), "moves the clock");
        assertArrayEquals(Headers.le32(Headers.CLOCK_BITS), hardest(c), "and leaves the hardest: it is not harder");
        assertEquals(0x6A93, header(c, Headers.mine(T0 + 120, HALF_BITS + 1)), "a target a hair over twice: weak");
        assertEquals(0x9000, header(c, grown(T0 + 180)), "a harder header");
        assertArrayEquals(Headers.le32(GROWN_BITS), hardest(c), "is the hardest now");
        Map<String, Object> before = c.persistent();
        assertEquals(0x6A93, header(c, net(T0 + 240)), "and CLOCK_BITS, a hair over twice its target, is weak");
        assertEquals(before, c.persistent());
        assertEquals(0x9000, header(c, Headers.mine(T0 + 300, 0x203FFFFEL)), "exactly twice its target is not");
        assertEquals(T0 + 300, now(c));
    }

    @Test
    void beforeAnyHardestWorkUnderTwiceTheFloorsIsWeak() throws Exception {
        CardTestSupport c = new CardTestSupport();
        Map<String, Object> before = c.persistent();
        assertEquals(0x6A93, header(c, floorAt(T0)), "the floor's work");
        assertEquals(0x6A93, header(c, Headers.mine(T0, 0x20400000L)), "a hair under twice the floor's work");
        assertEquals(before, c.persistent(), "nothing written");
        assertEquals(0, anchored(c), "and nothing anchored");
        assertEquals(0x9000, header(c, net(T0)), "just over twice the floor's work (CLOCK_BITS) is not weak");
        assertEquals(T0, now(c));
        assertArrayEquals(Headers.le32(Headers.CLOCK_BITS), hardest(c));
    }

    @Test
    void anOlderHeaderThatIsNotWeakMovesNothingAfterTheSetUp() throws Exception {
        CardTestSupport c = card(0, net(T0), 0, 0);
        assertEquals(0x9000, header(c, net(T0 + DAY)));
        c.select();
        assertEquals(0x9000, header(c, grown(T0 + HOUR)), "an older header, harder");
        assertEquals(T0 + DAY, now(c), "never brings the clock back");
        assertArrayEquals(Headers.le32(Headers.CLOCK_BITS), hardest(c), "nor raises the hardest");
        assertEquals(0, anchored(c), "and anchors nothing once a PIN is set");
    }

    @Test
    void aWeakFarHeaderAgainAndAgainGivesNoDay() throws Exception {
        CardTestSupport c = card(1000, net(T0), 3, 1000);
        assertEquals(0x9000, pay(c, 0));
        for (int round = 0; round < 3; round++) {
            c.select();
            assertEquals(0x6A93, header(c, floorAt(F + round)), "round " + round);
            assertEquals(0x6A95, pay(c, 1), "round " + round + ": the day's limit is spent");
        }
        assertEquals(1000, spent(c), "one day's limit, and no more");
        assertEquals(T0, now(c));
    }
}
