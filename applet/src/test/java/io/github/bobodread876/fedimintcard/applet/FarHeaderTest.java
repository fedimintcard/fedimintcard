package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.ClockTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Map;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * A header far ahead, adapted from Cashu card 3.1's FarHeaderTest (D199, D200). This card has no owner and no reset, so
 * nothing brings a clock back after the set-up. What it does: a far header at the floor's work is weak and refused
 * (WeakClockTest); one with the network's own work is a clock like any other, and its day ends only when real time gets
 * there (the residual the design accepts); a day whose end does not fit four bytes never ends, the safe side; and a header
 * refused for its work writes nothing, so real headers after it move the clock as before.
 */
class FarHeaderTest {

    @AfterEach
    void theBuiltFloor() { Headers.useTheBuiltFloor(); }

    @Test
    void aFarHeaderWithTheNetworksWorkIsAClockLikeAnyOther() throws Exception {
        Headers.useTheTestFloor();
        CardTestSupport c = card(1000, net(T0), 3, 1000);
        assertEquals(0x9000, pay(c, 0));
        c.select();
        assertEquals(0x9000, header(c, net(F)), "a far header with the network's work");
        assertEquals(F, now(c));
        assertEquals(0x9000, pay(c, 1), "the day is over by it, and a new one begins at it");
        assertEquals(F, window(c));
        assertEquals(0x9000, header(c, net(T0 + 10 * DAY)), "a real header, older");
        assertEquals(F, now(c), "moves nothing");
        assertEquals(0x6A95, pay(c, 2), "the note waits for that day's end (the cost accepted)");
    }

    @Test
    void aDayPastWhatFourBytesHoldNeverEnds() throws Exception {
        Headers.useTheTestFloor();
        CardTestSupport c = card(1000, net(T0), 2, 1000);
        assertEquals(0x9000, header(c, net(TOP)));
        assertEquals(0x9000, pay(c, 0));
        assertEquals(TOP, window(c));
        assertEquals(0x9000, header(c, net(0xFFFFFFFFL)));
        assertEquals(0x6A95, pay(c, 1), "the day is never read as over");
    }

    @Test
    void aHeaderRefusedForItsWorkWritesNothingAndRealHeadersMoveTheClockAfterIt() throws Exception {
        CardTestSupport c = card(0, Headers.H900000, 0, 0);
        assertEquals(Headers.T900000, now(c));
        c.select();
        Map<String, Object> before = c.persistent();
        assertEquals(0x6A93, header(c, OfflineTest.GENESIS), "under the floor");
        byte[] forged = Headers.H900200.clone();
        forged[79] ^= 1;   // another nonce: the hash no longer meets the target
        assertEquals(0x6A93, header(c, forged), "without its own work");
        byte[] far = Headers.H900200.clone();
        System.arraycopy(Headers.le32(0xFFFFFF00L), 0, far, 68, 4);
        assertEquals(0x6A93, header(c, far), "a real header with a far time: its hash no longer meets the target");
        assertEquals(before, c.persistent(), "nothing written");
        assertEquals(0x9000, header(c, Headers.H900200));
        assertEquals(1749309742L, now(c), "a real header after them moves the clock");
    }
}
