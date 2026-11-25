package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import io.github.bobodread876.fedimintcard.applet.Headers;
import java.util.Arrays;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * A header far ahead at the floor's work, sent with no PIN, is weak: the card answers 6A93 and changes nothing, and real
 * headers after it still move the clock and end days.
 *
 * <p>REFLECTION: the applet's FLOOR_BITS is lowered to the tests' floor (0x207FFFFF, half of all hashes), as Cashu card
 * 3.1's FarHeaderTest does, so the far header can be mined here. Every other check of SET_HEADER runs as built. The card is
 * set up at a header of CLOCK_BITS (Headers.anchor), which stands for the network's blocks.
 */
class FarHeaderRefusedTest {
    /** Real mainnet blocks 900 000 and 900 200 (from OfflineTest), June 2025, bits 0x17023774. */
    static final byte[] H900000 = Headers.H900000;
    static final byte[] H900200 = Headers.H900200;

    static final long LIMIT = 10_000_000L;     // 10 000 sat a day
    static final long NOTE = 6_000_000L;       // so two notes fit no day
    static final long T0 = 1_800_000_000L;     // 2027
    static final long DAY = 86_400L;
    /** So far ahead that a day past it does not fit four bytes. */
    static final long TOP = 0xFFFFFF00L;

    @BeforeEach
    void theTestFloor() { Headers.useTheTestFloor(); }

    @AfterEach
    void theBuiltFloor() { Headers.useTheBuiltFloor(); }

    /** A provisioned card: limit, PIN (after its anchor), four notes of NOTE each. */
    static SimCard card() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.sw(SET_LIMIT, 0, be64(LIMIT)));
        assertEquals(0x9000, c.setPin());
        for (int s = 0; s < 4; s++) {
            assertEquals(0x9000, c.sw(GEN_KEY, s, null));
            assertEquals(0x9000, c.sw(SET_NOTE, s, c.note(s, NOTE)));
        }
        return c;
    }

    static int spend(SimCard c, int slot) {
        c.powerCycle();
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        return c.sw(SIGN, slot, SimCard.sha256(new byte[] { (byte) slot }));
    }

    @Test
    void aFarHeaderAtFloorWorkIsRefusedAndChangesNothing() throws Exception {
        SimCard c = card();
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(T0, CLOCK_BITS)), "the holder's ordinary clock");
        assertEquals(0x9000, spend(c, 0));
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(T0 + DAY + 60, CLOCK_BITS)));
        assertEquals(0x9000, spend(c, 1), "a new day: ordinary");
        assertEquals(T0 + DAY + 60, c.window());

        c.powerCycle();   // the stranger: no PIN, a floor-work header at the top of the clock
        byte[] clock = ((byte[]) c.field("clock")).clone();
        assertEquals(0x6A93, c.sw(SET_HEADER, 0, mine(TOP, TEST_BITS)));
        assertArrayEquals(clock, (byte[]) c.field("clock"), "the clock record as it was");
        assertEquals(T0 + DAY + 60, c.now());

        assertEquals(0x6A95, spend(c, 2), "the same day: two notes do not fit it");
        assertEquals(T0 + DAY + 60, c.window());
        assertEquals(0x9000, c.sw(SET_HEADER, 0, mine(T0 + 2 * DAY + 120, CLOCK_BITS)), "a day on");
        assertEquals(0x9000, spend(c, 2), "the day ends as before");
        assertEquals(T0 + 2 * DAY + 120, c.window());
        assertEquals(NOTE, c.spentToday());
    }

    @Test
    void realHeadersAfterARefusedFarHeaderStillMoveTheClockAndEndDays() throws Exception {
        SimCard c = card();
        c.powerCycle();
        assertEquals(0x6A93, c.sw(SET_HEADER, 0, mine(TOP, TEST_BITS)), "the stranger's header first: refused");
        assertEquals(Headers.ANCHOR_TIME, c.now());
        assertEquals(0x9000, c.sw(SET_HEADER, 0, H900000), "a real block");
        assertEquals(Headers.T900000, c.now(), "moves the clock");
        assertEquals(0x9000, spend(c, 0));
        assertEquals(0x6A95, spend(c, 1), "the same day");
        assertEquals(0x9000, c.sw(SET_HEADER, 0, H900200), "121 243 s later");
        assertEquals(1749309742L, c.now());
        assertEquals(0x9000, spend(c, 1), "a new day");
        byte[] bits = (byte[]) c.field("clock");
        assertArrayEquals(Arrays.copyOfRange(H900000, 72, 76), Arrays.copyOfRange(bits, 8, 12),
            "the hardest is the real blocks' bits");
    }
}
