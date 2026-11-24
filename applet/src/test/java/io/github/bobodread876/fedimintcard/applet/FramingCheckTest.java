package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.math.BigInteger;
import javacard.framework.ISOException;
import javacard.security.ECPrivateKey;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

/**
 * The signer's engine check ({@link SchnorrHW#checkFraming}), which SET_PIN runs before it reads or writes anything:
 * the chip's key agreement and point addition must answer known points in the framing and with the Y the signer
 * expects, or the card refuses 6F00. Each engine is put behind a watched one that answers in a named wrong way.
 */
class FramingCheckTest {

    static final BigInteger FP = CashuTestSupport.SECP_P;
    static final BigInteger FN = CashuTestSupport.SECP_N;
    static final byte[] SECP_G = point(CashuTestSupport.SECP_GX, CashuTestSupport.SECP_GY);

    static Object hidden(SchnorrHW s, String name) throws Exception {
        java.lang.reflect.Field f = SchnorrHW.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(s);
    }

    static void setHidden(SchnorrHW s, String name, Object value) throws Exception {
        java.lang.reflect.Field f = SchnorrHW.class.getDeclaredField(name);
        f.setAccessible(true);
        f.set(s, value);
    }

    static byte[] u32(BigInteger v) {
        byte[] raw = v.toByteArray(), out = new byte[32];
        int len = Math.min(raw.length, 32);
        System.arraycopy(raw, raw.length - len, out, 32 - len, len);
        return out;
    }

    static byte[] point(BigInteger x, BigInteger y) {
        byte[] out = new byte[65];
        out[0] = 4;
        System.arraycopy(u32(x), 0, out, 1, 32);
        System.arraycopy(u32(y), 0, out, 33, 32);
        return out;
    }

    /** The root (x^3 + 7)^((p+1)/4) mod p: the Y an engine that works Y out again by the plain square root answers. */
    static BigInteger plainRoot(BigInteger x) {
        return x.pow(3).add(BigInteger.valueOf(7)).mod(FP).modPow(FP.add(BigInteger.ONE).shiftRight(2), FP);
    }

    /** [k]G, 04 || X || Y. */
    static byte[] timesG(long k) {
        BigInteger[] w = CashuTestSupport.ecMulTest(BigInteger.valueOf(k), CashuTestSupport.SECP_GX, CashuTestSupport.SECP_GY);
        return point(w[0], w[1]);
    }

    private static int refusal(Runnable r) {
        try { r.run(); } catch (ISOException e) { return e.getReason() & 0xFFFF; }
        return 0x9000;
    }


    static final class WatchedAgreement extends javacard.security.KeyAgreement {
        final javacard.security.KeyAgreement real;
        final java.util.List<byte[]> given = new java.util.ArrayList<>();
        String frame = "";
        /** A member of the family of wrong engines (FAMILY_IN), or null: {base, scalar, answer} on the key agreement, {generator, point added, scalar, middle point, answer} on the point addition. */
        String[] model;
        boolean fail, refuseInit;
        /** The call (1 for the first) at which it throws, 0 for none; and the call whose answer's Y it negates, 0 for none. */
        int failOn, negateOn;

        WatchedAgreement(byte alg) { real = javacard.security.KeyAgreement.getInstance(alg, false); }

        /** The generator of each key it is initialised with (the point the generic mapping multiplies). */
        final java.util.List<byte[]> generators = new java.util.ArrayList<>();
        final java.util.List<byte[]> scalars = new java.util.ArrayList<>();
        /** The scalar of the key at each point it is given (as `given`). */
        final java.util.List<BigInteger> askedBy = new java.util.ArrayList<>();
        @Override public void init(javacard.security.PrivateKey k) {
            if (refuseInit) javacard.security.CryptoException.throwIt(javacard.security.CryptoException.ILLEGAL_VALUE);
            if (k instanceof ECPrivateKey && k.isInitialized()) {
                byte[] g = new byte[65];
                short n = ((ECPrivateKey) k).getG(g, (short) 0);
                generators.add(java.util.Arrays.copyOf(g, n));
                byte[] sk = new byte[32];
                ((ECPrivateKey) k).getS(sk, (short) 0);
                scalars.add(sk);
                byte[] s = new byte[33];
                lastS = new BigInteger(1, java.util.Arrays.copyOf(s, ((ECPrivateKey) k).getS(s, (short) 0)));
                lastG = n == 65 ? xy(g, 0) : null;
            }
            real.init(k);
        }
        /** The scalar and generator of the key it was last initialised with. */
        BigInteger lastS;
        BigInteger[] lastG;
        @Override public byte getAlgorithm() { return real.getAlgorithm(); }
        @Override public short generateSecret(byte[] pub, short off, short len, byte[] out, short outOff) {
            given.add(java.util.Arrays.copyOfRange(pub, off, off + len));
            askedBy.add(lastS);
            if (fail || given.size() == failOn) javacard.security.CryptoException.throwIt(javacard.security.CryptoException.ILLEGAL_USE);
            boolean gm = real.getAlgorithm() == javacard.security.KeyAgreement.ALG_EC_PACE_GM;
            if (given.size() == negateOn) {
                byte[] full = new byte[65];
                real.generateSecret(pub, off, len, full, (short) 0);
                System.arraycopy(negate(full), 0, out, outOff, 65);
                return 65;
            }
            String[] m = model != null ? model : named(frame, gm);
            if (m != null && len == 65) {
                // worked out here, by the model's rules, from the key's scalar and generator and the point given
                BigInteger[] r = modelAnswer(m, lastS, gm ? lastG : null, xy(pub, off));
                System.arraycopy(r == null ? new byte[] { 4 } : point(r[0], r[1]), 0, out, outOff, r == null ? 1 : 65);
                if (r == null) java.util.Arrays.fill(out, outOff + 1, outOff + 65, (byte) 0);
                return 65;
            }
            byte[] full = new byte[65];
            short n = real.generateSecret(pub, off, len, full, (short) 0);
            BigInteger fy = new BigInteger(1, java.util.Arrays.copyOfRange(full, 33, 65));
            switch (frame) {
                case "hyb": System.arraycopy(full, 0, out, outOff, 65); out[outOff] = (byte) (6 | (fy.testBit(0) ? 1 : 0)); return 65;
                case "comp65": System.arraycopy(full, 0, out, outOff, 65); out[outOff] = (byte) (2 | (fy.testBit(0) ? 1 : 0)); return 65;
                case "ret64": System.arraycopy(full, 0, out, outOff, 65); return 64;
                case "ret66": System.arraycopy(full, 0, out, outOff, 65); return 66;
                case "bare": System.arraycopy(full, 1, out, outOff, 64); return 64;
                case "lead00": System.arraycopy(full, 0, out, outOff, 65); out[outOff] = 0; return 65;
                case "short": System.arraycopy(full, 0, out, outOff, 33); return 33;
                case "yx": out[outOff] = 4; System.arraycopy(full, 33, out, outOff + 1, 32); System.arraycopy(full, 1, out, outOff + 33, 32); return 65;
                case "other": System.arraycopy(timesG(3), 0, out, outOff, 65); return 65;
                case "negy": {
                    System.arraycopy(full, 0, out, outOff, 33);
                    System.arraycopy(u32(FP.subtract(fy)), 0, out, outOff + 33, 32);
                    return 65;
                }
                default: System.arraycopy(full, 0, out, outOff, n); return n;
            }
        }
    }

    /** The framings and the named wrong engines WatchedAgreement can answer as (see its comment). */
    static final String[] FRAMINGS = { "bare", "lead00", "short", "yx", "other", "negy", "hyb", "comp65", "ret64", "ret66" };
    static final String[] WRONG_ENGINES = { "even", "root", "negin", "odd", "negroot", "xbit", "hi", "inEven", "notG", "hOdd", "hNegroot", "hXbit", "inXbit", "evenHi" };

    /** A named wrong engine as a member of the family: {base, scalar, answer} (the key agreement) or {generator, point added, scalar, middle, answer} (the point addition); null for none. */
    static String[] named(String how, boolean gm) {
        switch (how) {
            case "even": case "odd": case "root": case "negroot": case "xbit": case "notG":
                return gm ? new String[] { "id", "id", "id", "id", how } : new String[] { "id", "id", how };
            case "negin": return gm ? new String[] { "id", "neg", "id", "id", "id" } : new String[] { "neg", "id", "id" };
            case "hi": return gm ? new String[] { "id", "id", "hi", "id", "id" } : new String[] { "id", "hi", "id" };
            case "inEven": return gm ? new String[] { "even", "id", "id", "id", "id" } : new String[] { "even", "id", "id" };
            case "inXbit": return gm ? new String[] { "xbit", "id", "id", "id", "id" } : new String[] { "xbit", "id", "id" };
            case "evenHi": return gm ? new String[] { "even", "id", "hi", "id", "id" } : new String[] { "even", "hi", "id" };
            case "hOdd": return gm ? new String[] { "id", "odd", "id", "id", "id" } : new String[] { "odd", "id", "id" };
            case "hNegroot": return gm ? new String[] { "id", "negroot", "id", "id", "id" } : new String[] { "negroot", "id", "id" };
            case "hXbit": return gm ? new String[] { "id", "xbit", "id", "id", "id" } : new String[] { "xbit", "id", "id" };
            default: return null;
        }
    }

    private static final java.util.Map<String, BigInteger[]> TIMES = new java.util.concurrent.ConcurrentHashMap<>();

    /** [k]p, null for the point at infinity. */
    static BigInteger[] times(BigInteger k, BigInteger[] p) {
        if (p == null || k.mod(FN).signum() == 0) return null;
        String key = k.mod(FN).toString(16) + ":" + p[0].toString(16) + ":" + p[1].toString(16);
        BigInteger[] r = TIMES.get(key);
        if (r == null) {
            r = CashuTestSupport.ecMulTest(k.mod(FN), p[0], p[1]);
            TIMES.put(key, r);
        }
        return r;
    }

    /** a + b, null for the point at infinity. */
    static BigInteger[] plus(BigInteger[] a, BigInteger[] b) {
        if (a == null) return b;
        if (b == null) return a;
        return CashuTestSupport.ecAddTest(a[0], a[1], b[0], b[1]);
    }

    private static boolean isG(BigInteger[] q) {
        return q != null && q[0].equals(CashuTestSupport.SECP_GX) && q[1].equals(CashuTestSupport.SECP_GY);
    }

    /** The point q (X kept) with the Y the rule gives it; s the scalar asked, in the point given, gen the generator (null for a point given). */
    static BigInteger[] lift(String rule, BigInteger[] q, BigInteger s, BigInteger[] in, BigInteger[] gen) {
        if (q == null) return null;
        BigInteger x = q[0], r = plainRoot(x);
        int odd;
        switch (rule) {
            case "id": return q;
            case "neg": return new BigInteger[] { x, FP.subtract(q[1]).mod(FP) };
            case "notG": return isG(gen == null ? q : gen) ? q : new BigInteger[] { x, FP.subtract(q[1]).mod(FP) };
            case "root": return new BigInteger[] { x, r };
            case "negroot": return new BigInteger[] { x, FP.subtract(r) };
            case "even": odd = 0; break;
            case "odd": odd = 1; break;
            case "xbit": odd = x.testBit(0) ? 1 : 0; break;
            case "nxbit": odd = x.testBit(0) ? 0 : 1; break;
            case "sp": odd = s.testBit(0) ? 1 : 0; break;
            case "nsp": odd = s.testBit(0) ? 0 : 1; break;
            case "hib": odd = s.testBit(255) ? 1 : 0; break;
            case "ip": odd = in[1].testBit(0) ? 1 : 0; break;
            case "nip": odd = in[1].testBit(0) ? 0 : 1; break;
            case "gp": odd = gen[1].testBit(0) ? 1 : 0; break;
            case "ngp": odd = gen[1].testBit(0) ? 0 : 1; break;
            case "ixor": odd = in[1].testBit(0) != s.testBit(0) ? 1 : 0; break;
            default: throw new IllegalArgumentException(rule);
        }
        return new BigInteger[] { x, r.testBit(0) == (odd == 1) ? r : FP.subtract(r) };
    }

    /** The scalar the rule works with in place of s. */
    static BigInteger scalarBy(String rule, BigInteger s) {
        switch (rule) {
            case "id": case "modn": return s.mod(FN);
            case "hi": return s.compareTo(FN.shiftRight(1)) > 0 ? FN.subtract(s) : s;
            case "lo": return s.compareTo(FN.shiftRight(1)) < 0 ? FN.subtract(s) : s;
            default: throw new IllegalArgumentException(rule);
        }
    }

    /** The answer of the model m: the key agreement ({base, scalar, answer}, g null) for [s]p; the point addition ({generator, point added, scalar, middle, answer}) for [s]g + p. Null for the point at infinity. */
    static BigInteger[] modelAnswer(String[] m, BigInteger s, BigInteger[] g, BigInteger[] p) {
        if (m.length == 3) return lift(m[2], times(scalarBy(m[1], s), lift(m[0], p, s, p, null)), s, p, p);
        BigInteger[] mid = lift(m[3], times(scalarBy(m[2], s), lift(m[0], g, s, g, null)), s, g, g);
        return lift(m[4], plus(mid, lift(m[1], p, s, p, null)), s, p, g);
    }

    /** Where `want` is in `list`, or -1. */
    static int indexOf(java.util.List<byte[]> list, byte[] want) {
        for (int i = 0; i < list.size(); i++) if (java.util.Arrays.equals(list.get(i), want)) return i;
        return -1;
    }

    /** X and Y of the 65-byte point at b[off]. */
    static BigInteger[] xy(byte[] b, int off) {
        return new BigInteger[] { new BigInteger(1, java.util.Arrays.copyOfRange(b, off + 1, off + 33)), new BigInteger(1, java.util.Arrays.copyOfRange(b, off + 33, off + 65)) };
    }

    /** The even one of the two roots of x^3 + 7. */
    static BigInteger evenRoot(BigInteger x) {
        BigInteger y = plainRoot(x);
        return y.testBit(0) ? FP.subtract(y) : y;
    }

    /** a + b on the curve. */
    static BigInteger[] sum(BigInteger[] a, BigInteger[] b) {
        return CashuTestSupport.ecAddTest(a[0], a[1], b[0], b[1]);
    }

    /** The 65-byte point p with its Y made p - Y. */
    static byte[] negate(byte[] p) {
        BigInteger[] q = xy(p, 0);
        return point(q[0], FP.subtract(q[1]));
    }

    @Test
    @DisplayName("The framing check (asked at SET_PIN) refuses a chip whose key agreement frames its answer otherwise: [11]G answered as the bare 64 bytes X || Y, as 65 bytes that do not begin 04, as 33 bytes, as 04 || Y || X, as 04 and the coordinates of another point, or with X right and Y not, is refused 6F00; the real framing passes, the check asked the key agreement for [11]G, [n - 1]([11]G), [2](-G) and [n - 1](2G) first, nine points in all, and leaves nothing in the temporary key")
    void theFramingCheckRefusesAnotherFraming() throws Exception {
        for (String frame : new String[] { "bare", "lead00", "short", "yx", "other", "negy" }) {
            SchnorrHW schnorr = new SchnorrHW();
            WatchedAgreement chip = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
            chip.frame = frame;
            setHidden(schnorr, "ecdh", chip);
            schnorr.init();
            ISOException thrown = org.junit.jupiter.api.Assertions.assertThrows(ISOException.class, () -> schnorr.checkFraming(new byte[65], (short) 0), "a framing \"" + frame + "\" must be refused");
            assertEquals(Secp256k1.SW_CRYPTO_ERROR, thrown.getReason());
            ECPrivateKey tmp = (ECPrivateKey) hidden(schnorr, "tmpPriv");
            if (tmp.isInitialized()) {
                byte[] s = new byte[32];
                tmp.getS(s, (short) 0);
                org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(SECP_G, 1, 33), s, "the refused check leaves G's x in the key, not 11");
            }
        }
        SchnorrHW schnorr = new SchnorrHW();
        WatchedAgreement chip = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
        setHidden(schnorr, "ecdh", chip);
        schnorr.init();
        assertEquals(0, chip.given.size(), "the install asks the chip for nothing");
        schnorr.checkFraming(new byte[65], (short) 0);
        assertEquals(9, chip.given.size(), "the check asked the key agreement for nine points (the last five: G, 2G, G, 2G and G)");
        org.junit.jupiter.api.Assertions.assertArrayEquals(SECP_G, chip.given.get(0), "the first of G");
        assertEquals(BigInteger.valueOf(11), chip.askedBy.get(0), "and that was [11]G");
        org.junit.jupiter.api.Assertions.assertArrayEquals(timesG(11), chip.given.get(1), "the second of [11]G, a point other than G");
        assertEquals(FN.subtract(BigInteger.ONE), chip.askedBy.get(1), "by n - 1, a scalar above n/2");
        org.junit.jupiter.api.Assertions.assertArrayEquals(negate(SECP_G), chip.given.get(2), "the third of -G, whose X and Y parities differ");
        assertEquals(BigInteger.TWO, chip.askedBy.get(2), "by 2");
        org.junit.jupiter.api.Assertions.assertArrayEquals(timesG(2), chip.given.get(3), "the fourth of 2G, a point other than G whose Y is even and the plain root");
        assertEquals(FN.subtract(BigInteger.ONE), chip.askedBy.get(3), "by n - 1");
    }


    @Test
    @DisplayName("The framing check asks the point addition too: [13]G + (-[2]G), two distinct points, by the generic mapping answered as the bare 64 bytes, as 65 bytes that do not begin 04, as 33 bytes, as 04 || Y || X, as another point, or with X right and Y not, is refused 6F00 and leaves G's x in the key; the real framing passes, and the point addition was asked eight times, first [13]G + (-[2]G), [n - 2]([11]G) + [11]G with its key's generator [11]G, and [4](-G) + 2G with its key's generator -G, X and Y")
    void theFramingCheckAsksThePointAdditionToo() throws Exception {
        for (String frame : new String[] { "bare", "lead00", "short", "yx", "other", "negy" }) {
            SchnorrHW schnorr = new SchnorrHW();
            WatchedAgreement gm = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_PACE_GM);
            gm.frame = frame;
            setHidden(schnorr, "gm", gm);
            schnorr.init();
            assertEquals(Secp256k1.SW_CRYPTO_ERROR & 0xFFFF, refusal(() -> schnorr.checkFraming(new byte[65], (short) 0)), "a point addition framed \"" + frame + "\"");
            ECPrivateKey tmp = (ECPrivateKey) hidden(schnorr, "tmpPriv");
            if (tmp.isInitialized()) {
                byte[] s = new byte[32];
                tmp.getS(s, (short) 0);
                org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(SECP_G, 1, 33), s, "the refused check leaves G's x in the key, not 13");
            }
        }
        SchnorrHW schnorr = new SchnorrHW();
        WatchedAgreement gm = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_PACE_GM);
        setHidden(schnorr, "gm", gm);
        schnorr.init();
        byte[] out = new byte[65];
        schnorr.checkFraming(out, (short) 0);
        assertEquals(8, gm.given.size(), "the point addition was asked eight times");
        byte[] twoG = timesG(2);
        org.junit.jupiter.api.Assertions.assertArrayEquals(point(new BigInteger(1, java.util.Arrays.copyOfRange(twoG, 1, 33)), FP.subtract(new BigInteger(1, java.util.Arrays.copyOfRange(twoG, 33, 65)))),
            gm.given.get(0), "first for -[2]G, two distinct points added");
        int thirteen = indexOf(gm.scalars, u32(BigInteger.valueOf(13))), nLess2 = indexOf(gm.scalars, u32(FN.subtract(BigInteger.TWO)));
        assertTrue(thirteen >= 0, "its key's scalar 13");
        org.junit.jupiter.api.Assertions.assertArrayEquals(SECP_G, gm.generators.get(thirteen), "with its key's generator G");
        org.junit.jupiter.api.Assertions.assertArrayEquals(timesG(11), gm.given.get(1), "then for [11]G");
        assertTrue(nLess2 > thirteen, "by its key's scalar n - 2, above n/2");
        org.junit.jupiter.api.Assertions.assertArrayEquals(timesG(11), gm.generators.get(nLess2), "with its key's generator [11]G, a point other than G");
        int four = indexOf(gm.scalars, u32(BigInteger.valueOf(4)));
        assertTrue(four > nLess2, "then by its key's scalar 4");
        org.junit.jupiter.api.Assertions.assertArrayEquals(negate(SECP_G), gm.generators.get(four), "with its key's generator -G");
        org.junit.jupiter.api.Assertions.assertArrayEquals(timesG(2), gm.given.get(2), "for 2G, a point added whose Y is even and the plain root");
    }

    @Test
    @DisplayName("The framing check holds the whole answer of both engines, X and Y, at [11]G, a point whose Y is odd and is not the plain root (x^3 + 7)^((p+1)/4): a key agreement or a point addition that answers the even root, that works Y out again by the plain root, or that negates the Y of each point it is given is refused 6F00, each engine alone and both together, and leaves G's x in the key")
    void theFramingCheckHoldsTheYAnEngineCouldGetWrong() throws Exception {
        byte[] eleven = timesG(11);
        BigInteger x11 = new BigInteger(1, java.util.Arrays.copyOfRange(eleven, 1, 33)), y11 = new BigInteger(1, java.util.Arrays.copyOfRange(eleven, 33, 65));
        assertTrue(y11.testBit(0), "[11]G's Y is odd: an engine that answers the even root gets it wrong");
        assertFalse(y11.equals(plainRoot(x11)), "[11]G's Y is not the plain root: an engine that works Y out by it gets it wrong");
        byte[] two = timesG(2);
        assertFalse(java.util.Arrays.equals(timesG(15), eleven), "[13]G + [2]G (H's Y negated) is not [11]G");
        for (String how : new String[] { "even", "root", "negin" }) {
            for (int which = 0; which < 3; which++) {
                SchnorrHW schnorr = new SchnorrHW();
                WatchedAgreement ecdh = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
                WatchedAgreement gm = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_PACE_GM);
                if (which != 1) ecdh.frame = how;
                if (which != 0) gm.frame = how;
                setHidden(schnorr, "ecdh", ecdh);
                setHidden(schnorr, "gm", gm);
                schnorr.init();
                String what = how + (which == 0 ? ", the key agreement" : which == 1 ? ", the point addition" : ", both");
                assertEquals(Secp256k1.SW_CRYPTO_ERROR & 0xFFFF, refusal(() -> schnorr.checkFraming(new byte[65], (short) 0)), what + ": refused");
                ECPrivateKey tmp = (ECPrivateKey) hidden(schnorr, "tmpPriv");
                if (tmp.isInitialized()) {
                    byte[] k = new byte[32];
                    tmp.getS(k, (short) 0);
                    org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(SECP_G, 1, 33), k, what + ": G's x left in the key");
                }
            }
        }
        assertTrue(two[64] % 2 == 0, "(and the old points' Y, 2G's and G's, are even and the plain root, which these engines answer right)");
        assertTrue(new BigInteger(1, java.util.Arrays.copyOfRange(two, 33, 65)).equals(plainRoot(new BigInteger(1, java.util.Arrays.copyOfRange(two, 1, 33)))));
    }

    @Test
    @DisplayName("The framing check refuses 6F00 every named wrong engine, on the key agreement alone, the point addition alone and both, at the ask the points were chosen for, and leaves G's x in the key: each framing (bare, lead00, short, yx, other, negy, hyb, comp65, ret64, ret66) and each engine that gets the point wrong (even, root, negin, odd, negroot, xbit, hi, inEven, notG, hOdd, hNegroot, hXbit, inXbit, evenHi). The key agreement is asked [11]G = 11G, [n - 1](11G) = -11G, [2](-G) = -2G and [n - 1](2G) = -2G; the point addition [13]G + (-2G) = 11G, [n - 2](11G) + 11G = -11G with the generator 11G, and [4](-G) + 2G = -2G with the generator -G; the real engines pass")
    void theFramingCheckRefusesEveryEngineModel() throws Exception {
        BigInteger[] e11 = xy(timesG(11), 0), m11 = { e11[0], FP.subtract(e11[1]) };
        assertTrue(e11[1].testBit(0) && !e11[1].equals(plainRoot(e11[0])), "[11]G: Y odd and not the plain root");
        assertTrue(!m11[1].testBit(0) && m11[1].equals(plainRoot(m11[0])) && m11[0].testBit(0), "-[11]G: Y even, the plain root, and X odd");
        BigInteger[] g = xy(SECP_G, 0), mg = { g[0], FP.subtract(g[1]) }, two = xy(timesG(2), 0);
        assertTrue(!mg[0].testBit(0) && mg[1].testBit(0) && !mg[1].equals(plainRoot(mg[0])), "-G: X even, Y odd and not the plain root (its parities differ)");
        assertTrue(two[0].testBit(0) && !two[1].testBit(0) && two[1].equals(plainRoot(two[0])), "2G: X odd, Y even and the plain root (its parities differ)");
        org.junit.jupiter.api.Assertions.assertArrayEquals(point(m11[0], m11[1]), point(times(FN.subtract(BigInteger.ONE), e11)[0], times(FN.subtract(BigInteger.ONE), e11)[1]), "[n - 1](11G) is -11G");
        BigInteger[] b = times(FN.subtract(BigInteger.TWO), e11);
        assertFalse(b[0].equals(e11[0]), "[n - 2]([11]G) is not [11]G or -[11]G: two distinct points are added");
        org.junit.jupiter.api.Assertions.assertArrayEquals(point(m11[0], m11[1]), point(sum(b, e11)[0], sum(b, e11)[1]), "[n - 2]([11]G) + [11]G is -[11]G");
        org.junit.jupiter.api.Assertions.assertArrayEquals(negate(timesG(2)), point(times(BigInteger.TWO, mg)[0], times(BigInteger.TWO, mg)[1]), "[2](-G) is -2G");
        BigInteger[] f = times(BigInteger.valueOf(4), mg);
        assertFalse(f[0].equals(two[0]), "[4](-G) is not 2G or -2G: two distinct points are added");
        org.junit.jupiter.api.Assertions.assertArrayEquals(negate(timesG(2)), point(sum(f, two)[0], sum(f, two)[1]), "[4](-G) + 2G is -2G");
        org.junit.jupiter.api.Assertions.assertArrayEquals(negate(timesG(2)), point(times(FN.subtract(BigInteger.ONE), two)[0], times(FN.subtract(BigInteger.ONE), two)[1]), "[n - 1](2G) is -2G");
        assertTrue(FN.subtract(BigInteger.TWO).compareTo(FN.shiftRight(1)) > 0, "n - 1 and n - 2 are above n/2");
        // the ask of each engine at which each is refused: the key agreement's, then the point addition's
        java.util.Map<String, int[]> want = new java.util.LinkedHashMap<>();
        for (String f2 : FRAMINGS) want.put(f2, new int[] { 1, 1 });
        for (String m : new String[] { "even", "root", "negin" }) want.put(m, new int[] { 1, 1 });
        for (String m : new String[] { "odd", "negroot", "xbit", "hi", "inEven", "notG" }) want.put(m, new int[] { 2, 2 });
        for (String m : new String[] { "hOdd", "hNegroot" }) want.put(m, new int[] { 1, 3 });
        for (String m : new String[] { "hXbit", "inXbit", "evenHi" }) want.put(m, new int[] { 3, 3 });
        assertEquals(FRAMINGS.length + WRONG_ENGINES.length, want.size(), "every framing and named engine has its row");
        StringBuilder table = new StringBuilder(String.format("%-9s | %-15s | %-15s | %s%n", "model", "key agreement", "point addition", "both"));
        for (java.util.Map.Entry<String, int[]> row : want.entrySet()) {
            String how = row.getKey();
            String[] cells = new String[3];
            for (int which = 0; which < 3; which++) {
                SchnorrHW schnorr = new SchnorrHW();
                WatchedAgreement ecdh = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
                WatchedAgreement gm = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_PACE_GM);
                if (which != 1) ecdh.frame = how;
                if (which != 0) gm.frame = how;
                setHidden(schnorr, "ecdh", ecdh);
                setHidden(schnorr, "gm", gm);
                schnorr.init();
                String what = how + (which == 0 ? ", the key agreement" : which == 1 ? ", the point addition" : ", both");
                assertEquals(Secp256k1.SW_CRYPTO_ERROR & 0xFFFF, refusal(() -> schnorr.checkFraming(new byte[65], (short) 0)), what + ": refused");
                ECPrivateKey tmp = (ECPrivateKey) hidden(schnorr, "tmpPriv");
                if (tmp.isInitialized()) {
                    byte[] k = new byte[32];
                    tmp.getS(k, (short) 0);
                    org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(SECP_G, 1, 33), k, what + ": G's x left in the key");
                }
                if (which < 2) {
                    int asked = which == 0 ? ecdh.given.size() : gm.given.size();
                    assertEquals(row.getValue()[which], asked, what + ": refused at that engine's ask " + row.getValue()[which]);
                    cells[which] = "refused, ask " + asked;
                } else {
                    cells[which] = "refused";
                }
            }
            table.append(String.format("%-9s | %-15s | %-15s | %s%n", how, cells[0], cells[1], cells[2]));
        }
        SchnorrHW schnorr = new SchnorrHW();
        WatchedAgreement ecdh = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
        WatchedAgreement gm = new WatchedAgreement(javacard.security.KeyAgreement.ALG_EC_PACE_GM);
        setHidden(schnorr, "ecdh", ecdh);
        setHidden(schnorr, "gm", gm);
        schnorr.init();
        schnorr.checkFraming(new byte[65], (short) 0);
        assertEquals(9, ecdh.given.size(), "the real engines pass: the key agreement asked nine times");
        assertEquals(8, gm.given.size(), "and the point addition eight times");
        table.append(String.format("%-9s | %-15s | %-15s | %s%n", "right", "passes", "passes", "passes"));
        System.out.print(table);
    }
}
