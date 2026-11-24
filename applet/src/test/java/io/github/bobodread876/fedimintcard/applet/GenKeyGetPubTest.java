package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.HashSet;
import java.util.Set;
import javax.smartcardio.ResponseAPDU;
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
        for (int s = 0; s < 8; s++) {
            byte[] pub = c.genKey(s);
            BigInteger d = c.scalar(s);
            assertTrue(d.signum() > 0 && d.compareTo(Bip340.N) < 0);
            assertArrayEquals(Bip340.bytes32(Bip340.pubkey(d)[0]), pub, "slot " + s);
            assertNotNull(Bip340.liftX(new BigInteger(1, pub)), "x is on the curve");
        }
    }

    @Test
    void everySlotGivesADistinctKey() {
        CardTestSupport c = new CardTestSupport();
        Set<String> seen = new HashSet<>();
        for (int s = 0; s < FedimintCardApplet.SLOTS; s++) assertTrue(seen.add(Bip340.hex(c.genKey(s))));
        assertEquals(FedimintCardApplet.SLOTS, seen.size());
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
        for (int s = 0; s < 8; s++) assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, s, null));
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
        assertTrue(Bip340.verify(pub, msg, c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg)));
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
            assertTrue(Bip340.verify(x, msg, c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg)), "d=" + d);
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

    @Test
    void overwriteGivesNewKeyAndOldKeyNoLongerVerifies() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] oldPub = c.genKey(7);
        BigInteger oldD = c.scalar(7);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        ResponseAPDU r = c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 7, 1, null);
        assertSw(0x9000, r);
        byte[] newPub = r.getData();
        assertEquals(32, newPub.length);
        assertFalse(java.util.Arrays.equals(oldPub, newPub));
        assertArrayEquals(newPub, c.getPub(7));
        BigInteger newD = c.scalar(7);
        assertNotEquals(oldD, newD);
        assertArrayEquals(Bip340.bytes32(Bip340.pubkey(newD)[0]), newPub);
        byte[] msg = CardTestSupport.fill(32, 9);
        byte[] sig = c.signPin(CardTestSupport.DEFAULT_PIN, 7, msg);
        assertTrue(Bip340.verify(newPub, msg, sig));
        assertFalse(Bip340.verify(oldPub, msg, sig), "old pubkey must not verify");
        // Neither response carries a scalar, old or new.
        java.util.List<byte[]> secrets = new java.util.ArrayList<>();
        for (BigInteger d : new BigInteger[] { oldD, newD }) {
            secrets.add(Bip340.bytes32(d));
            secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
        }
        assertFalse(NoPrivateKeyLeakTest.leaks(r.getBytes(), secrets));
        assertFalse(NoPrivateKeyLeakTest.leaks(sig, secrets));
        // A plain GEN_KEY is still refused afterwards.
        assertSw(0x6985, c.tx(CardTestSupport.GEN_KEY, 7, null));
    }

    @Test
    void overwriteOnEmptySlotWorks() {
        CardTestSupport c = new CardTestSupport();
        ResponseAPDU r = c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 6, 1, null);
        assertSw(0x9000, r);
        assertArrayEquals(r.getData(), c.getPub(6));
    }

    @Test
    void genKeyP2Of2IsRefused() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6B00, c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 0, 2, null));
        c.genKey(0);
        assertSw(0x6B00, c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 0, 2, null));
    }

    @Test
    void failedOverwriteLeavesSlotEmpty() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        javacard.security.ECPrivateKey[] privs = (javacard.security.ECPrivateKey[]) c.field("privs");
        javacard.security.ECPrivateKey real = privs[0];
        privs[0] = new SchnorrHWSignTest.LyingKey(real, false, (short) 32) {
            @Override public void setS(byte[] b, short o, short l) {
                javacard.framework.ISOException.throwIt((short) 0x6F00);
            }
        };
        assertSw(0x6F00, c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 0, 1, null));
        assertFalse(real.isInitialized(), "old private key must be wiped");
        assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, 0, null));
    }

    @Test
    void overwriteWhereGenKeyPairThrowsLeavesSlotEmpty() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        javacard.security.ECPrivateKey real = ((javacard.security.ECPrivateKey[]) c.field("privs"))[0];
        java.lang.reflect.Field f = FedimintCardApplet.class.getDeclaredField("gen");
        f.setAccessible(true);
        // KeyPair is final and jCardSim's genKeyPair cannot be made to throw, so break the generator
        // itself: the failure then happens in the generate path, after the old key was wiped.
        f.set(c.runtime.appletAt(c.aid), null);
        boolean refused;
        try {
            refused = c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 0, 1, null).getSW() != 0x9000;
        } catch (RuntimeException e) {
            refused = true;
        }
        assertTrue(refused, "genKeyPair must fail");
        assertFalse(real.isInitialized(), "old private key must be wiped");
        assertSw(0x6985, c.tx(CardTestSupport.GET_PUB, 0, null));
    }

    /** Once a PIN is set, replacing a used key needs it: not entered, the key stays. */
    @Test
    void overwriteOfAUsedSlotNeedsThePin() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(3);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.select();   // the selection that set the PIN counts as having it; a new one does not
        assertSw(0x6A94, c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 3, 1, null));
        assertArrayEquals(pub, c.getPub(3));
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        ResponseAPDU r = c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 3, 1, null);
        assertSw(0x9000, r);
        assertFalse(java.util.Arrays.equals(pub, r.getData()));
    }

    /** Wrong tries never let a reader re-key a slot: without this session's PIN, its keys stay. */
    @Test
    void overwriteAfterWrongTriesIsRefused() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(2);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        byte[] wrong = { '9', '9', '9', '9' };
        for (int i = 0; i < 3; i++) c.verifyPinHeld(wrong);
        assertSw(0x6A94, c.tx(CardTestSupport.CLA, CardTestSupport.GEN_KEY, 2, 1, null));
        assertArrayEquals(pub, c.getPub(2));
    }

    /**
     * A chip that gives a slot's scalar back as 33 bytes (a sign byte before it): the signer refuses such a key at every
     * SIGN, so GEN_KEY reads the key back and refuses it, 6F00, and the slot stays EMPTY. No note can reach it.
     */
    @Test
    void aKeyTheChipGivesBackLongerThan32BytesIsRefused() throws Exception {
        CardTestSupport c = new CardTestSupport();
        javacard.security.ECPrivateKey[] privs = (javacard.security.ECPrivateKey[]) c.field("privs");
        // REFLECTION: the slot's key objects built as GEN_KEY builds them, then one in front of the private key
        c.tx(CardTestSupport.GEN_KEY, 3, null);
        javacard.security.ECPrivateKey real = privs[3];
        assertNotNull(real);
        privs[3] = (javacard.security.ECPrivateKey) java.lang.reflect.Proxy.newProxyInstance(getClass().getClassLoader(),
            new Class<?>[] { javacard.security.ECPrivateKey.class }, (proxy, m, args) -> {
                if (m.getName().equals("getS")) {
                    byte[] out = (byte[]) args[0];
                    short o = (Short) args[1];
                    byte[] s32 = new byte[32];
                    real.getS(s32, (short) 0);
                    out[o] = 0;
                    System.arraycopy(s32, 0, out, o + 1, 32);
                    return (short) 33;
                }
                try {
                    return m.invoke(real, args);
                } catch (java.lang.reflect.InvocationTargetException e) {
                    throw e.getCause();
                }
            });
        ((byte[]) c.field("state"))[3] = (byte) CardTestSupport.ST_EMPTY;
        assertSw(0x6F00, c.tx(CardTestSupport.GEN_KEY, 3, null));
        assertEquals(CardTestSupport.ST_EMPTY, c.slotState(3), "the slot stays EMPTY");
        assertSw(0x6985, c.tx(CardTestSupport.SET_NOTE, 3, c.note(3, 1000)), "and takes no note");
        privs[3] = real;
        c.genKey(3);
    }
}
