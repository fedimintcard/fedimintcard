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

    @Test
    void compressedGetWStillGivesSameXOnly() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        javacard.security.ECPublicKey[] pubs = (javacard.security.ECPublicKey[]) c.field("pubs");
        pubs[0] = new SchnorrHWSignTest.CompressedPub(pubs[0]);
        assertArrayEquals(pub, c.getPub(0));
        byte[] msg = CardTestSupport.fill(32, 5);
        assertTrue(Bip340.verify(pub, msg, c.sign(0, msg)));
    }

    @Test
    void compressedBothParitiesSign() throws Exception {
        CardTestSupport c = new CardTestSupport();
        javacard.security.ECPublicKey[] pubs = (javacard.security.ECPublicKey[]) c.field("pubs");
        for (int d : new int[] { 3, 6 }) {   // even-y, odd-y
            c.installKey(0, BigInteger.valueOf(d));
            pubs[0] = new SchnorrHWSignTest.CompressedPub(pubs[0]);
            byte[] x = Bip340.bytes32(Bip340.pubkey(BigInteger.valueOf(d))[0]);
            assertArrayEquals(x, c.getPub(0));
            byte[] msg = CardTestSupport.fill(32, d);
            assertTrue(Bip340.verify(x, msg, c.sign(0, msg)), "d=" + d);
            pubs[0] = ((SchnorrHWSignTest.CompressedPub) pubs[0]).d;
        }
    }

    /** A setS that throws mid-GEN_KEY must leave neither tmp nor the generator key holding d. */
    @Test
    void failedGenKeyLeavesNoScalarBehind() throws Exception {
        CardTestSupport c = new CardTestSupport();
        javacard.security.ECPrivateKey[] privs = (javacard.security.ECPrivateKey[]) c.field("privs");
        privs[0] = new SchnorrHWSignTest.LyingKey(privs[0], false, (short) 32) {
            @Override public void setS(byte[] b, short o, short l) {
                javacard.framework.ISOException.throwIt((short) 0x6F00);
            }
        };
        assertSw(0x6F00, c.tx(CardTestSupport.GEN_KEY, 0, null));
        for (byte b : (byte[]) c.field("tmp")) assertEquals(0, b, "tmp must be zero");
        javacard.security.KeyPair gen = (javacard.security.KeyPair) c.field("gen");
        assertFalse(gen.getPrivate().isInitialized(), "generator private key must be cleared");
        assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, 0, null));   // slot not marked used
    }
}
