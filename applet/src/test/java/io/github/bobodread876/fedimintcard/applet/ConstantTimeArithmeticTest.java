package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.Random;
import org.junit.jupiter.api.Test;

/**
 * The constant-time arithmetic of F1 (docs/OFFLINE-PLAN.md) against BigInteger: the edges where a
 * masked subtraction or a carry fold could go wrong, then random values. And signatures by many
 * random keys, so both parities and the key split are exercised over and over.
 */
class ConstantTimeArithmeticTest {
    static final BigInteger N = Bip340.N;
    static final BigInteger TWO256 = BigInteger.ONE.shiftLeft(256);

    static BigInteger read(byte[] a, int off) {
        byte[] b = new byte[32];
        System.arraycopy(a, off, b, 0, 32);
        return new BigInteger(1, b);
    }

    static final BigInteger[] EDGES = {
        BigInteger.ZERO, BigInteger.ONE, BigInteger.valueOf(2), N.subtract(BigInteger.ONE), N.subtract(BigInteger.TWO),
        N.shiftRight(1), N.shiftRight(1).add(BigInteger.ONE), TWO256.subtract(N), TWO256.subtract(N).subtract(BigInteger.ONE),
    };

    static BigInteger[] values() {
        Random r = new Random(0xF1);
        BigInteger[] v = new BigInteger[EDGES.length + 300];
        System.arraycopy(EDGES, 0, v, 0, EDGES.length);
        for (int i = EDGES.length; i < v.length; i++) v[i] = new BigInteger(256, r).mod(N);
        return v;
    }

    @Test
    void addModNIsTheSumModN() {
        byte[] work = new byte[SchnorrHW.WORK_LEN];
        byte[] out = new byte[32];
        BigInteger[] v = values();
        for (int i = 0; i < v.length; i++) {
            BigInteger a = v[i], b = v[(i * 7 + 3) % v.length];
            SchnorrHW.addModN(Bip340.bytes32(a), (short) 0, Bip340.bytes32(b), (short) 0, out, (short) 0, work, (short) 288);
            assertEquals(a.add(b).mod(N), read(out, 0), a.toString(16) + " + " + b.toString(16));
        }
        // b may be n itself, as d2 = d + (n - d1) is when d1 is 0
        for (BigInteger a : EDGES) {
            SchnorrHW.addModN(Bip340.bytes32(a), (short) 0, Bip340.bytes32(N), (short) 0, out, (short) 0, work, (short) 288);
            assertEquals(a.mod(N), read(out, 0));
        }
    }

    @Test
    void reduceModNTakesNOffEveryValueBelowTwoN() {
        byte[] work = new byte[SchnorrHW.WORK_LEN];
        BigInteger[] tops = { N, N.add(BigInteger.ONE), TWO256.subtract(BigInteger.ONE), N.subtract(BigInteger.ONE), BigInteger.ZERO };
        for (BigInteger x : tops) {
            byte[] a = Bip340.bytes32(x);
            SchnorrHW.reduceModN(a, (short) 0, work, (short) 288);
            assertEquals(x.mod(N), read(a, 0), x.toString(16));
        }
    }

    @Test
    void mulModNIsTheProductModN() {
        byte[] work = new byte[SchnorrHW.WORK_LEN];
        byte[] out = new byte[32];
        BigInteger[] v = values();
        for (int i = 0; i < v.length; i++) {
            BigInteger a = v[i], b = v[(i * 13 + 5) % v.length];
            SchnorrHW.mulModN(Bip340.bytes32(a), (short) 0, Bip340.bytes32(b), (short) 0, out, (short) 0, work, (short) 0);
            assertEquals(a.multiply(b).mod(N), read(out, 0), a.toString(16) + " * " + b.toString(16));
        }
    }

    @Test
    void condNegateTakesNMinusAOnlyUnderTheMask() {
        byte[] work = new byte[SchnorrHW.WORK_LEN];
        for (BigInteger x : values()) {
            if (x.signum() == 0) continue;
            byte[] a = Bip340.bytes32(x);
            SchnorrHW.condNegateModN(a, (short) 0, (byte) 0x00, work, (short) 288);
            assertEquals(x, read(a, 0));
            SchnorrHW.condNegateModN(a, (short) 0, (byte) 0xFF, work, (short) 288);
            assertEquals(N.subtract(x), read(a, 0));
        }
    }

    @Test
    void manyRandomKeysSignAndVerify() {
        SchnorrHW h = SchnorrHWSignTest.signer();
        Random r = new Random(0x51);
        int odd = 0;
        for (int i = 0; i < 200; i++) {
            BigInteger d = new BigInteger(256, r).mod(N.subtract(BigInteger.ONE)).add(BigInteger.ONE);
            byte[] msg = new byte[32];
            r.nextBytes(msg);
            BigInteger[] pt = Bip340.pubkey(d);
            if (pt[1].testBit(0)) odd++;
            byte[] sig = new byte[64];
            h.sign(SchnorrHWSignTest.priv(d), SchnorrHWSignTest.pub(d), msg, (short) 0, sig, (short) 0);
            assertTrue(Bip340.verify(Bip340.bytes32(pt[0]), msg, sig), "key " + d.toString(16));
        }
        assertTrue(odd > 50 && odd < 150, "both parities, about half each: " + odd);
    }

    @Test
    void theMessageMayShareTheOutputBuffer() {
        // The applet signs from the APDU buffer into the APDU buffer; Rx now goes out before s.
        SchnorrHW h = SchnorrHWSignTest.signer();
        BigInteger d = SchnorrHWSignTest.KEYS[4];
        byte[] buf = new byte[80];
        for (int i = 0; i < 32; i++) buf[5 + i] = (byte) (0x33 + i);
        byte[] msg = java.util.Arrays.copyOfRange(buf, 5, 37);
        h.sign(SchnorrHWSignTest.priv(d), SchnorrHWSignTest.pub(d), buf, (short) 5, buf, (short) 0);
        assertTrue(Bip340.verify(Bip340.bytes32(Bip340.pubkey(d)[0]), msg, java.util.Arrays.copyOf(buf, 64)));
    }
}
