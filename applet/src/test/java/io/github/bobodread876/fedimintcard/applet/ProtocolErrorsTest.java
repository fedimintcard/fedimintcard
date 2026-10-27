package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class ProtocolErrorsTest {
    @Test
    void wrongClaIs6E00() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6E00, c.tx(0x80, 0x10, 0, 0, null));
        assertSw(0x6E00, c.tx(0x00, 0x11, 0, 0, null));
    }

    @Test
    void unknownInsIs6D00() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6D00, c.tx(0xB0, 0x13, 0, 0, null));
        assertSw(0x6D00, c.tx(0xB0, 0x00, 0, 0, null));
    }

    @Test
    void badP1P2Is6B00() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6B00, c.tx(0xB0, 0x10, 8, 0, null));
        assertSw(0x6B00, c.tx(0xB0, 0x10, 8, 1, null));
        assertSw(0x6B00, c.tx(0xB0, 0x10, 0, 2, null));
        assertSw(0x6B00, c.tx(0xB0, 0x10, 0, 0xFF, null));
        assertSw(0x6B00, c.tx(0xB0, 0x11, 0xFF, 0, null));
        assertSw(0x6B00, c.tx(0xB0, 0x11, 0, 1, null));
        assertSw(0x6B00, c.tx(0xB0, 0x12, 0, 1, new byte[32]));
        assertSw(0x6B00, c.tx(0xB0, 0x12, 0, 2, new byte[32]));
    }

    @Test
    void selectIs9000WithNoData() {
        CardTestSupport c = new CardTestSupport();
        javax.smartcardio.ResponseAPDU r = c.sim.transmitCommand(new javax.smartcardio.CommandAPDU(
            0x00, 0xA4, 0x04, 0x00, Bip340.hex(CardTestSupport.AID_HEX)));
        assertSw(0x9000, r);
        assertEquals(0, r.getData().length);
    }

    @Test
    void genKeyAndGetPubWithDataIs6700() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6700, c.tx(0xB0, 0x10, 0, 0, new byte[1]));
        c.genKey(0);
        assertSw(0x6700, c.tx(0xB0, 0x11, 0, 0, new byte[1]));
    }

    @Test
    void keysSurviveCardReset() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(7);
        c.sim.reset();
        assertSw(0x9000, c.sim.transmitCommand(new javax.smartcardio.CommandAPDU(
            0x00, 0xA4, 0x04, 0x00, Bip340.hex(CardTestSupport.AID_HEX))));
        assertArrayEquals(pub, c.getPub(7));
    }
}
