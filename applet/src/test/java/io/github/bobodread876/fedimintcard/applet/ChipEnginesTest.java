package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CashuTestSupport.*;
import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.math.BigInteger;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Random;
import javacard.security.CryptoException;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.Key;
import javacard.security.KeyBuilder;
import javacard.security.RSAPrivateKey;
import javacardx.crypto.Cipher;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

/**
 * D214, D215, D216 (and D213 beside them): what the card asks of the chip's own engines, and what it does where a chip does
 * not hold to them.
 *
 *   D214  the field's arithmetic on the chip's RSA with p itself, padded to 64 bytes, for the modulus: the answer is under p at
 *         once (no reduction by bytecode); keys made and held to known answers at SET_CARD, the p*p keys kept as the fallback;
 *   D215  LOAD_CHANGE's K, C_ and the unblinded C held to the curve by the chip's key agreement (D212's), where the set-up's
 *         test of the agreement held;
 *   D216  e*d mod n in the signature by two squarings on the RSA engine (modulus n padded), 4ab = (a+b)^2 - (a-b)^2, its inputs
 *         reduced below n first, the bytecode multiplication kept as the fallback; its known answers held at SET_CARD.
 *
 * jCardSim refuses a padded modulus ("not composite"), so a test of the engine path puts an engine of its own in the card's
 * place: `Engine` computes with BigInteger what the chip's RSA does (a power mod the key's modulus, 64 bytes in and out), and,
 * as the chip does, answers garbage for an input at or over the modulus. The card makes its own keys' moduli and exponents
 * into the keys it is given (`Key`), so the engine can only be right if the card set them right.
 */
class ChipEnginesTest {

    static final BigInteger N = new BigInteger("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", 16);
    static final BigInteger GX = new BigInteger("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798", 16);
    static final BigInteger GY = new BigInteger("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8", 16);

    // =========================================================================
    // The chip's RSA, as a test has it
    // =========================================================================

    /** An RSA key that keeps whatever modulus and exponent the card sets in it. */
    static final class FakeKey implements RSAPrivateKey {
        BigInteger mod, exp;
        int modLen;
        public void setModulus(byte[] b, short o, short l) { mod = new BigInteger(1, Arrays.copyOfRange(b, o, o + l)); modLen = l; }
        public void setExponent(byte[] b, short o, short l) { exp = new BigInteger(1, Arrays.copyOfRange(b, o, o + l)); }
        public short getModulus(byte[] b, short o) { return 0; }
        public short getExponent(byte[] b, short o) { return 0; }
        public boolean isInitialized() { return mod != null && exp != null; }
        public void clearKey() { mod = null; exp = null; }
        public byte getType() { return KeyBuilder.TYPE_RSA_PRIVATE; }
        public short getSize() { return KeyBuilder.LENGTH_RSA_512; }
    }

    /**
     * The chip's RSA for a FakeKey: x^e mod m for the 64 bytes in, 64 bytes out; for x at or over m, garbage (what the chip
     * answers: no error). `lie` makes it wrong (the last byte of every answer changed), `refuse` makes every use throw. A key
     * that is not a FakeKey goes to the real engine (the simulator's), if there is one.
     */
    static class Engine extends Cipher {
        final Cipher real;
        FakeKey key;
        int powers, oldPowers, garbage;
        boolean lie, refuse;
        /** Every power's key modulus, in order. */
        final List<BigInteger> moduli = new ArrayList<>();
        Engine(Cipher real) { this.real = real; }
        @Override public void init(Key k, byte m) throws CryptoException {
            if (k instanceof FakeKey) { key = (FakeKey) k; return; }
            key = null;
            real.init(k, m);
        }
        @Override public void init(Key k, byte m, byte[] b, short o, short l) throws CryptoException { init(k, m); }
        @Override public byte getAlgorithm() { return Cipher.ALG_RSA_NOPAD; }
        @Override public byte getPaddingAlgorithm() { return Cipher.PAD_NULL; }
        @Override public byte getCipherAlgorithm() { return Cipher.ALG_RSA_NOPAD; }
        @Override public short update(byte[] a, short o, short l, byte[] out, short oo) throws CryptoException { throw new UnsupportedOperationException(); }
        @Override public short doFinal(byte[] a, short o, short l, byte[] out, short oo) throws CryptoException {
            if (key == null) { oldPowers++; return real.doFinal(a, o, l, out, oo); }
            if (refuse) CryptoException.throwIt(CryptoException.ILLEGAL_VALUE);
            powers++;
            moduli.add(key.mod);
            assertEquals(64, l, "the chip's RSA takes the modulus' length");
            BigInteger x = new BigInteger(1, Arrays.copyOfRange(a, o, o + l)), r;
            if (x.compareTo(key.mod) >= 0) {
                garbage++;
                r = new BigInteger(1, sha256(a, o, l)).shiftLeft(256).xor(BigInteger.valueOf(0x5A5A5A5AL));
            } else {
                r = x.modPow(key.exp, key.mod);
            }
            if (lie) r = r.xor(BigInteger.ONE);
            byte[] b = r.toByteArray(), res = new byte[64];
            System.arraycopy(b, Math.max(0, b.length - 64), res, 64 - Math.min(64, b.length), Math.min(64, b.length));
            System.arraycopy(res, 0, out, oo, 64);
            return 64;
        }
        private static byte[] sha256(byte[] a, int o, int l) { return CashuTestSupport.sha256(Arrays.copyOfRange(a, o, o + l)); }
    }

    static Object get(Object owner, Class<?> cls, String name) throws Exception {
        java.lang.reflect.Field f = cls.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(owner);
    }
    static void set(Object owner, Class<?> cls, String name, Object v) throws Exception {
        java.lang.reflect.Field f = cls.getDeclaredField(name);
        f.setAccessible(true);
        f.set(owner, v);
    }
    static boolean flag(Object owner, Class<?> cls, String name) throws Exception { return (Boolean) get(owner, cls, name); }

    /** The two engines a card is given in place of the chip's: `field` for the p keys, `sign` for the signature's squaring. */
    static final class Engines {
        Engine field, sign;
    }

    // =========================================================================
    // D216: e * d mod n on the chip's RSA
    // =========================================================================

    static byte[] be(BigInteger v) { return unsigned32(v); }

    /** The signature engine on a signer of its own: `chipMulInit` run on `engine`'s FakeKey. */
    static SchnorrHW signerWith(Engine engine) throws Exception {
        SchnorrHW s = new SchnorrHW();
        s.init();
        set(s, SchnorrHW.class, "rsaC", engine);
        set(s, SchnorrHW.class, "sqKey", new FakeKey());
        return s;
    }

    @Test
    @DisplayName("D216: the chip's multiplication mod n (two squarings, 4ab = (a+b)^2 - (a-b)^2, two halvings) against Python's: 340 products (0, 1, 2, n - 1, n - 2, n/2, (n+1)/2, 2^255 and the like in every pairing, a*a, and random ones), each the same as a * b mod n; the engine is asked for exactly two squarings, both of inputs under n (it answers garbage over the modulus, and none was given)")
    void theChipsProductsAreRight() throws Exception {
        Engine eng = new Engine(null);
        SchnorrHW s = signerWith(eng);
        assertTrue(s.chipMulInit(), "the squaring's known answers hold");
        eng.powers = 0;
        eng.garbage = 0;
        BigInteger half = N.shiftRight(1);
        BigInteger[] special = { BigInteger.ZERO, BigInteger.ONE, BigInteger.TWO, N.subtract(BigInteger.ONE), N.subtract(BigInteger.TWO), half, half.add(BigInteger.ONE),
            BigInteger.ONE.shiftLeft(255), BigInteger.ONE.shiftLeft(128), GX, GY };
        List<BigInteger[]> pairs = new ArrayList<>();
        for (BigInteger a : special) for (BigInteger b : special) pairs.add(new BigInteger[] { a, b });
        Random rnd = new Random(340);
        while (pairs.size() < 340) pairs.add(new BigInteger[] { new BigInteger(256, rnd).mod(N), new BigInteger(256, rnd).mod(N) });
        byte[] w = new byte[256];
        int n = 0;
        for (BigInteger[] ab : pairs) {
            byte[] a = be(ab[0]), b = be(ab[1]), out = new byte[32];
            s.mulModNChip(a, (short) 0, b, (short) 0, out, (short) 0, w, (short) 0);
            assertEquals(ab[0].multiply(ab[1]).mod(N), new BigInteger(1, out), "a = " + ab[0].toString(16) + " b = " + ab[1].toString(16));
            n++;
        }
        assertEquals(340, n);
        assertEquals(0, eng.garbage, "never an input at or over n");
        assertEquals(2 * 340, eng.powers, "two squarings a product");
        for (BigInteger m : eng.moduli) assertEquals(N, m, "modulus n");
        // and a*a, in one place
        byte[] a = be(GX), out = new byte[32];
        s.mulModNChip(a, (short) 0, a, (short) 0, out, (short) 0, w, (short) 0);
        assertEquals(GX.multiply(GX).mod(N), new BigInteger(1, out));
    }

    @Test
    @DisplayName("D216: inputs at or over n (n, n + 1, 2^256 - 1: a 256-bit value can be over n) are reduced below n before the engine sees them: the product is the product of their residues, and the engine is never given a value at or over its modulus (it answers garbage there); the inputs are reduced where they are, as the doc says")
    void inputsOverNAreReducedFirst() throws Exception {
        Engine eng = new Engine(null);
        SchnorrHW s = signerWith(eng);
        assertTrue(s.chipMulInit());
        eng.powers = 0;
        BigInteger top = BigInteger.ONE.shiftLeft(256).subtract(BigInteger.ONE);
        BigInteger[] vals = { N, N.add(BigInteger.ONE), N.add(BigInteger.valueOf(12345)), top, top.subtract(BigInteger.ONE), BigInteger.valueOf(7) };
        byte[] w = new byte[256];
        for (BigInteger x : vals) {
            for (BigInteger y : vals) {
                byte[] a = be(x), b = be(y), out = new byte[32];
                s.mulModNChip(a, (short) 0, b, (short) 0, out, (short) 0, w, (short) 0);
                assertEquals(x.mod(N).multiply(y.mod(N)).mod(N), new BigInteger(1, out), x.toString(16) + " * " + y.toString(16));
            }
        }
        assertEquals(0, eng.garbage, "the chip was never handed a value at or over n");
    }

    @Test
    @DisplayName("D216: the self-test of the squaring (3^2 = 9, (n - 1)^2 = 1, G's x times G's y both ways round) holds on an engine that holds; an engine that lies, refuses, or is wrong only on a full-width value fails it, and chipMul is then false")
    void theSquaringSelfTest() throws Exception {
        for (String how : new String[] { "ok", "lie", "refuse", "wide" }) {
            Engine eng = new Engine(null) {
                @Override public short doFinal(byte[] a, short o, short l, byte[] out, short oo) {
                    short n = super.doFinal(a, o, l, out, oo);
                    if (key != null && how.equals("wide")) {
                        boolean big = false;
                        for (int i = 0; i < 16; i++) if (out[oo + 32 + i] != 0) big = true;
                        if (big) out[oo + 63] ^= 4;
                    }
                    return n;
                }
            };
            eng.lie = how.equals("lie");
            eng.refuse = how.equals("refuse");
            SchnorrHW s = signerWith(eng);
            assertEquals(how.equals("ok"), s.chipMulInit(), how);
            // and the self-test leaves nothing of itself in the signer's work or scratch
            assertTrue(isZero(get(s, SchnorrHW.class, "work"), 0), "work is clear after the self-test: " + how);
        }
    }
    static boolean isZero(Object arr, int from) {
        for (byte b : (byte[]) arr) if (b != 0) return false;
        return true;
    }

    private static ECPrivateKey ecPriv(byte[] d) {
        ECPrivateKey priv = (ECPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
        ECPublicKey pub = (ECPublicKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        Secp256k1.curve(priv);
        priv.setS(d, (short) 0, (short) 32);
        return priv;
    }
    private static ECPublicKey ecPub(byte[] w) {
        ECPublicKey pub = (ECPublicKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        Secp256k1.curve(pub);
        pub.setW(w, (short) 0, (short) 65);
        return pub;
    }

    @Test
    @DisplayName("D216, D217: BIP-340 signatures by the chip's multiplication verify: 30 signatures under keys of both P.y parities (and the date form, d + t), 0, 1 and n - 1 among the keys, over random messages; each verified with the BIP-340 reference (schnorrVerify), four squarings a signature (the split of D217), nothing of d or k left in the signer's work or scratch afterwards")
    void signaturesByTheChipVerify() throws Exception {
        Engine eng = new Engine(null);
        SchnorrHW s = signerWith(eng);
        assertTrue(s.chipMulInit());
        s.chipMul = true;
        eng.powers = 0;
        Random rnd = new Random(30);
        List<BigInteger> ds = new ArrayList<>(Arrays.asList(BigInteger.ONE, BigInteger.TWO, BigInteger.valueOf(3), BigInteger.valueOf(6), N.subtract(BigInteger.ONE), N.subtract(BigInteger.TWO)));
        while (ds.size() < 30) ds.add(new BigInteger(255, rnd).add(BigInteger.ONE));
        int odd = 0, even = 0;
        for (BigInteger d : ds) {
            BigInteger[] pt = ecMulTest(d, SECP_GX, SECP_GY);
            byte[] w = point65(pt);
            if (pt[1].testBit(0)) odd++; else even++;
            byte[] msg = new byte[32];
            rnd.nextBytes(msg);
            byte[] sig = new byte[64];
            assertEquals(64, s.sign(ecPriv(be(d)), ecPub(w), msg, (short) 0, sig, (short) 0));
            assertTrue(schnorrVerify(be(pt[0]), msg, sig), "the signature under d = " + d.toString(16));
            assertTrue(isZero(get(s, SchnorrHW.class, "sc"), 0), "sc is cleared after the signature");
            assertTrue(isZero(get(s, SchnorrHW.class, "work"), 0), "work is cleared after the signature");
        }
        assertTrue(odd > 3 && even > 3, "both parities: " + odd + " odd, " + even + " even");
        assertEquals(4 * ds.size(), eng.powers, "four squarings a signature (D217: two products, e*d1 and e*d2, of two each; D216 made it two)");
        assertEquals(0, eng.garbage, "of inputs under n alone");
        // keys per load: d + t, the public key P + t*G given after t
        BigInteger d = BigInteger.valueOf(0x123456789ABCDEFL), t = N.subtract(BigInteger.valueOf(5));
        BigInteger[] pd = ecMulTest(d.add(t).mod(N), SECP_GX, SECP_GY);
        byte[] g = concat(be(t), compressed(pd));
        byte[] msg = new byte[32];
        rnd.nextBytes(msg);
        byte[] sig = new byte[64];
        ECPrivateKey priv = ecPriv(be(d));
        // the card's own public key object is given too: D207's check makes Q = W + t*G from it and holds it to the point named
        assertEquals(64, s.sign(priv, ecPub(point65(ecMulTest(d, SECP_GX, SECP_GY))), g, (short) 0, msg, (short) 0, sig, (short) 0));
        assertTrue(schnorrVerify(be(pd[0]), msg, sig), "the date's key");
    }

    @Test
    @DisplayName("D216: with chipMul false (a failed self-test, or a chip with no such keys) the signature is by the bytecode multiplication, as before, and verifies; the engine is not asked for anything")
    void withoutTheChipTheBytecodeSigns() throws Exception {
        Engine eng = new Engine(null);
        SchnorrHW s = signerWith(eng);
        eng.refuse = true;
        assertFalse(s.chipMulInit(), "an engine that refuses");
        assertFalse(flag(s, SchnorrHW.class, "chipMul"));
        BigInteger d = BigInteger.valueOf(0xCAFEBABEL);
        BigInteger[] pt = ecMulTest(d, SECP_GX, SECP_GY);
        byte[] msg = new byte[32], sig = new byte[64];
        Arrays.fill(msg, (byte) 7);
        eng.powers = 0;
        assertEquals(64, s.sign(ecPriv(be(d)), ecPub(point65(pt)), msg, (short) 0, sig, (short) 0));
        assertTrue(schnorrVerify(be(pt[0]), msg, sig));
        assertEquals(0, eng.powers);
    }

    @Test
    @DisplayName("D216: the signature through the chip is the same as the bytecode's for the same nonce: with the randomness fixed, s = k + e*d mod n is byte for byte the same either way")
    void theSameSignatureEitherWay() throws Exception {
        BigInteger d = BigInteger.valueOf(0x7777777777L).shiftLeft(100).add(BigInteger.valueOf(31337));
        BigInteger[] pt = ecMulTest(d, SECP_GX, SECP_GY);
        byte[] msg = new byte[32];
        Arrays.fill(msg, (byte) 0x42);
        byte[][] sigs = new byte[2][];
        for (int how = 0; how < 2; how++) {
            Engine eng = new Engine(null);
            SchnorrHW s = signerWith(eng);
            assertTrue(s.chipMulInit());
            s.chipMul = how == 1;
            // the same randomness: an RandomData that gives the same bytes every time
            set(s, SchnorrHW.class, "rng", new javacard.security.RandomData() {
                @Override public void generateData(byte[] b, short o, short l) { for (short i = 0; i < l; i++) b[(short)(o + i)] = (byte) (i + 1); }
                @Override public short nextBytes(byte[] b, short o, short l) { generateData(b, o, l); return (short)(o + l); }
                @Override public void setSeed(byte[] b, short o, short l) { }
                @Override public byte getAlgorithm() { return ALG_SECURE_RANDOM; }
            });
            byte[] sig = new byte[64];
            s.sign(ecPriv(be(d)), ecPub(point65(pt)), msg, (short) 0, sig, (short) 0);
            sigs[how] = sig;
            assertTrue(schnorrVerify(be(pt[0]), msg, sig));
        }
        assertArrayEquals(sigs[0], sigs[1], "the same signature, bytecode and chip");
    }
}
