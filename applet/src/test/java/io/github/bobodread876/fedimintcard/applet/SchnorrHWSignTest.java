package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;
import javacard.framework.ISOException;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyBuilder;
import org.junit.jupiter.api.Test;

/** Drives SchnorrHW directly with chosen keys (ported in spirit from cashucard's test). */
class SchnorrHWSignTest {
    static final byte[] P = Bip340.bytes32(Bip340.P);
    static final byte[] A = new byte[32];
    static final byte[] B = Bip340.bytes32(BigInteger.valueOf(7));
    static final byte[] N = Bip340.bytes32(Bip340.N);
    static final byte[] G = Bip340.uncompressed(Bip340.g());

    static ECPrivateKey priv(BigInteger d) {
        ECPrivateKey k = (ECPrivateKey) KeyBuilder.buildKey(
            KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
        k.setFieldFP(P, (short) 0, (short) 32);
        k.setA(A, (short) 0, (short) 32);
        k.setB(B, (short) 0, (short) 32);
        k.setG(G, (short) 0, (short) 65);
        k.setR(N, (short) 0, (short) 32);
        k.setK((short) 1);
        byte[] s = Bip340.bytes32(d);
        k.setS(s, (short) 0, (short) 32);
        return k;
    }

    static ECPublicKey pub(BigInteger d) {
        ECPublicKey k = (ECPublicKey) KeyBuilder.buildKey(
            KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        k.setFieldFP(P, (short) 0, (short) 32);
        k.setA(A, (short) 0, (short) 32);
        k.setB(B, (short) 0, (short) 32);
        k.setG(G, (short) 0, (short) 65);
        k.setR(N, (short) 0, (short) 32);
        k.setK((short) 1);
        byte[] w = Bip340.uncompressed(Bip340.pubkey(d));
        k.setW(w, (short) 0, (short) 65);
        return k;
    }

    static SchnorrHW signer() {
        SchnorrHW h = new SchnorrHW(G, P, A, B, N);
        h.init();
        return h;
    }

    static final BigInteger[] KEYS = {
        BigInteger.valueOf(2), BigInteger.valueOf(3), BigInteger.valueOf(6), BigInteger.valueOf(9),
        new BigInteger("B7E151628AED2A6ABF7158809CF4F3C762E7160F38B4DA56A784D9045190CFEF", 16),
        new BigInteger("0B432B2677937381AEF05BB02A66ECD012773062CF3FA2549E44F58ED2401710", 16) };

    @Test
    void signsForBothParities() {
        SchnorrHW h = signer();
        byte[] msg = new byte[32];
        for (int i = 0; i < 32; i++) msg[i] = (byte) (0x5A ^ i);
        int odd = 0, even = 0;
        for (BigInteger d : KEYS) {
            BigInteger[] pt = Bip340.pubkey(d);
            if (pt[1].testBit(0)) odd++; else even++;
            byte[] sig = new byte[64];
            assertEquals(64, h.sign(priv(d), pub(d), msg, (short) 0, sig, (short) 0));
            assertTrue(Bip340.verify(Bip340.bytes32(pt[0]), msg, sig), "d=" + d.toString(16));
        }
        assertTrue(odd > 0 && even > 0);
    }

    @Test
    void signUsesAFreshNonce() {
        SchnorrHW h = signer();
        BigInteger d = KEYS[4];
        byte[] msg = Bip340.hex("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
        Set<String> rs = new HashSet<>();
        for (int i = 0; i < 24; i++) {
            byte[] sig = new byte[64];
            h.sign(priv(d), pub(d), msg, (short) 0, sig, (short) 0);
            assertTrue(Bip340.verify(Bip340.bytes32(Bip340.pubkey(d)[0]), msg, sig));
            assertTrue(rs.add(Bip340.hex(Arrays.copyOf(sig, 32))), "R repeated");
        }
    }

    /** Wraps a private key and lies about getS's length. */
    static class LyingKey implements ECPrivateKey {
        final ECPrivateKey d;
        final boolean strip;
        final short fixed;
        LyingKey(ECPrivateKey d, boolean strip, short fixed) { this.d = d; this.strip = strip; this.fixed = fixed; }
        public short getS(byte[] b, short o) {
            byte[] full = new byte[32];
            short len = d.getS(full, (short) 0);
            if (!strip) { System.arraycopy(full, 0, b, o, len); return fixed; }
            short lead = 0;
            while (lead < len - 1 && full[lead] == 0) lead++;
            System.arraycopy(full, lead, b, o, len - lead);
            return (short) (len - lead);
        }
        public void setS(byte[] b, short o, short l) { d.setS(b, o, l); }
        public void setFieldFP(byte[] b, short o, short l) { d.setFieldFP(b, o, l); }
        public void setFieldF2M(short e) { d.setFieldF2M(e); }
        public void setFieldF2M(short e1, short e2, short e3) { d.setFieldF2M(e1, e2, e3); }
        public void setA(byte[] b, short o, short l) { d.setA(b, o, l); }
        public void setB(byte[] b, short o, short l) { d.setB(b, o, l); }
        public void setG(byte[] b, short o, short l) { d.setG(b, o, l); }
        public void setR(byte[] b, short o, short l) { d.setR(b, o, l); }
        public void setK(short k) { d.setK(k); }
        public short getField(byte[] b, short o) { return d.getField(b, o); }
        public short getA(byte[] b, short o) { return d.getA(b, o); }
        public short getB(byte[] b, short o) { return d.getB(b, o); }
        public short getG(byte[] b, short o) { return d.getG(b, o); }
        public short getR(byte[] b, short o) { return d.getR(b, o); }
        public short getK() { return d.getK(); }
        public void copyDomainParametersFrom(javacard.security.ECKey s) { d.copyDomainParametersFrom(s); }
        public boolean isInitialized() { return d.isInitialized(); }
        public void clearKey() { d.clearKey(); }
        public byte getType() { return d.getType(); }
        public short getSize() { return d.getSize(); }
    }

    @Test
    void signRightAlignsShortPrivateScalar() {
        SchnorrHW h = signer();
        byte[] msg = Bip340.hex("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
        // dirty the scratchpad with a full-width key first
        h.sign(priv(KEYS[4]), pub(KEYS[4]), msg, (short) 0, new byte[64], (short) 0);
        BigInteger d = BigInteger.valueOf(3);
        LyingKey strip = new LyingKey(priv(d), true, (short) 0);
        assertEquals(1, strip.getS(new byte[32], (short) 0), "stand-in really reports a short length");
        byte[] sig = new byte[64];
        assertEquals(64, h.sign(strip, pub(d), msg, (short) 0, sig, (short) 0));
        assertTrue(Bip340.verify(Bip340.bytes32(Bip340.pubkey(d)[0]), msg, sig));
    }

    @Test
    void signRejectsOutOfRangePrivateScalarLength() {
        SchnorrHW h = signer();
        BigInteger d = KEYS[4];
        byte[] msg = new byte[32];
        for (short bogus : new short[] { 0, 33, -1 }) {
            LyingKey liar = new LyingKey(priv(d), false, bogus);
            ISOException e = assertThrows(ISOException.class,
                () -> h.sign(liar, pub(d), msg, (short) 0, new byte[64], (short) 0));
            assertEquals((short) 0x6F00, e.getReason());
        }
    }

    @Test
    void initRejectsMismatchedGroupOrder() {
        byte[] wrong = N.clone();
        wrong[31] ^= 1;
        ISOException e = assertThrows(ISOException.class,
            () -> new SchnorrHW(G, P, A, B, wrong).init());
        assertEquals((short) 0x6F00, e.getReason());
    }
}
