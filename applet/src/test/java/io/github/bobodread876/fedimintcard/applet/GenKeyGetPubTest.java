package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.HashSet;
import java.util.Set;
import org.junit.jupiter.api.Test;

class GenKeyGetPubTest {
    @Test
    void genKeyReturnsPubkeyMatchingGetPub() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        assertEquals(32, pub.length);
        assertArrayEquals(pub, c.getPub(0));
    }

    @Test
    void pubkeyIsScalarTimesG() throws Exception {
        CardTestSupport c = new CardTestSupport();
        for (int s = 0; s < 4; s++) {
            byte[] pub = c.genKey(s);
            BigInteger d = c.scalar(s);
            assertTrue(d.signum() > 0 && d.compareTo(Bip340.N) < 0);
            assertArrayEquals(Bip340.bytes32(Bip340.pubkey(d)[0]), pub, "slot " + s);
            assertNotNull(Bip340.liftX(new BigInteger(1, pub)), "x is on the curve");
        }
    }

    @Test
    void fourSlotsGiveFourDistinctKeys() {
        CardTestSupport c = new CardTestSupport();
        Set<String> seen = new HashSet<>();
        for (int s = 0; s < 4; s++) assertTrue(seen.add(Bip340.hex(c.genKey(s))));
        assertEquals(4, seen.size());
    }

    @Test
    void secondGenKeyOnUsedSlotIsRefusedAndKeyUnchanged() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(2);
        assertSw(0x6985, c.tx(CardTestSupport.GEN_KEY, 2, null));
        assertArrayEquals(pub, c.getPub(2));
    }

    @Test
    void getPubOnEmptySlotIsRefused() {
        CardTestSupport c = new CardTestSupport();
        for (int s = 0; s < 4; s++) assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, s, null));
        c.genKey(1);
        assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, 0, null));
    }
}
