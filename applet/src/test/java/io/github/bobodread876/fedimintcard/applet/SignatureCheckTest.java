package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.ISOException;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyAgreement;
import javacard.security.KeyBuilder;
import java.math.BigInteger;
import java.util.Arrays;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

import static io.github.bobodread876.fedimintcard.applet.CashuTestSupport.*;
import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * A signature is verified before it leaves the card (SchnorrHW.sign's step 8, BIP-340's "if Verify fails, abort").
 *
 * Why: d is in RAM and e*d is a multiplication in software, and AUTH signs for any reader, with no PIN, as often as it likes.
 * A fault in that multiplication, or one bit of d flipped, makes a signature whose error s'*G - R - e*P = e*delta*G leaves
 * few values for delta; enough of them give d. Here the faults are modelled as keys that do not agree (a public key object
 * naming another key, a private key one bit off, a date's point that is not P + t*G): each must be refused 6F00 with the
 * caller's buffer as it was and nothing of s in the signer's scratch. The waits are checked signatures too (D218, the keeper's
 * answer K1 of 2026-11-18, which amends D207's unchecked `signatureWork`: the holds keep the seconds measured with the check).
 * And on the chip's path (D216, D217) a signature the check refuses is made again, whole, by the bytecode with a fresh nonce
 * (D218, the merge plan's join of D207 and our check; M2 with the other maintainer is open).
 *
 * jCardSim's engines are the simulator's, not the chip's (D10): a scalar of 0, a sum at infinity and an engine's answer
 * written over its own input are cases this does not reach, and the check's time on a chip is not measured here.
 */
class SignatureCheckTest {

    static final BigInteger FN = SECP_N;

    static SchnorrHW signer() {
        SchnorrHW s = new SchnorrHW();
        s.init();
        return s;
    }

    static byte[] msg(int b) {
        byte[] m = new byte[32];
        for (int i = 0; i < 32; i++) m[i] = (byte) (b ^ (i * 7));
        return m;
    }

    static BigInteger[] times(BigInteger k) { return ecMulTest(k.mod(FN), SECP_GX, SECP_GY); }

    static byte[] point65(BigInteger[] w) { return sigPoint(w[0], w[1]); }

    /** The x of the 65-byte or 33-byte key, as BIP-340 names it. */
    static byte[] xOf(byte[] w) { return Arrays.copyOfRange(w, 1, 33); }

    /** `out`, 96 bytes of 0xA5: what a refused signature must leave as it found. */
    static byte[] untouched() {
        byte[] out = new byte[96];
        Arrays.fill(out, (byte) 0xA5);
        return out;
    }

    static int refusal(Runnable r) {
        try { r.run(); } catch (ISOException e) { return e.getReason() & 0xFFFF; }
        return 0x9000;
    }

    /** The signer's scratch and work, all zero: nothing of d, k, e*d or s is left. */
    static void leftNothing(SchnorrHW s, String what) throws Exception {
        assertTrue(sigAllZero((byte[]) sigHidden(s, "sc")), what + ": the scratch is zero");
        assertTrue(sigAllZero((byte[]) sigHidden(s, "work")), what + ": the work, s among it, is zero");
    }

    /**
     * A public key object that answers getW in the form a chip may give: "65" as 04 || X || Y, "33" as 02/03 || X, of the
     * point it was made with, whatever the simulator's own key would answer.
     */
    static final class ChipPublicKey implements ECPublicKey {
        final ECPublicKey real;
        final byte[] w65;
        final boolean compressed;
        ChipPublicKey(BigInteger[] w, boolean compressed) {
            this.w65 = point65(w);
            this.compressed = compressed;
            this.real = sigPubOf(toHex(w65));
        }
        @Override public short getW(byte[] b, short o) {
            if (!compressed) { System.arraycopy(w65, 0, b, o, 65); return 65; }
            b[o] = (byte) ((w65[64] & 1) == 0 ? 2 : 3);
            System.arraycopy(w65, 1, b, o + 1, 32);
            return 33;
        }
        @Override public void setW(byte[] b, short o, short l) { real.setW(b, o, l); }
        @Override public void setFieldFP(byte[] b, short o, short l) { real.setFieldFP(b, o, l); }
        @Override public void setFieldF2M(short e) { real.setFieldF2M(e); }
        @Override public void setFieldF2M(short e1, short e2, short e3) { real.setFieldF2M(e1, e2, e3); }
        @Override public void setA(byte[] b, short o, short l) { real.setA(b, o, l); }
        @Override public void setB(byte[] b, short o, short l) { real.setB(b, o, l); }
        @Override public void setG(byte[] b, short o, short l) { real.setG(b, o, l); }
        @Override public void setR(byte[] b, short o, short l) { real.setR(b, o, l); }
        @Override public void setK(short k) { real.setK(k); }
        @Override public short getField(byte[] b, short o) { return real.getField(b, o); }
        @Override public short getA(byte[] b, short o) { return real.getA(b, o); }
        @Override public short getB(byte[] b, short o) { return real.getB(b, o); }
        @Override public short getG(byte[] b, short o) { return real.getG(b, o); }
        @Override public short getR(byte[] b, short o) { return real.getR(b, o); }
        @Override public short getK() { return real.getK(); }
        @Override public void copyDomainParametersFrom(javacard.security.ECKey src) { real.copyDomainParametersFrom(src); }
        @Override public boolean isInitialized() { return true; }
        @Override public void clearKey() { real.clearKey(); }
        @Override public byte getType() { return real.getType(); }
        @Override public short getSize() { return real.getSize(); }
    }

    /** A key agreement in front of the simulator's that counts its calls and keeps each answer. */
    static final class Counted extends KeyAgreement {
        final KeyAgreement real;
        final java.util.List<byte[]> answers = new java.util.ArrayList<>();
        Counted(byte alg) { real = KeyAgreement.getInstance(alg, false); }
        @Override public void init(javacard.security.PrivateKey k) { real.init(k); }
        @Override public byte getAlgorithm() { return real.getAlgorithm(); }
        @Override public short generateSecret(byte[] pub, short off, short len, byte[] out, short outOff) {
            short n = real.generateSecret(pub, off, len, out, outOff);
            answers.add(Arrays.copyOfRange(out, outOff, outOff + n));
            return n;
        }
    }

    /** A signer whose two engines count their calls: {signer, the key agreement, the point addition}. */
    static Object[] countedSigner() throws Exception {
        SchnorrHW s = new SchnorrHW();
        Counted ecdh = new Counted(KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY), gm = new Counted(KeyAgreement.ALG_EC_PACE_GM);
        sigSetHidden(s, "ecdh", ecdh);
        sigSetHidden(s, "gm", gm);
        s.init();
        return new Object[] { s, ecdh, gm };
    }

    // =========================================================================
    // (a) the signatures that verify still do, and are checked on the card
    // =========================================================================

    @Test
    @DisplayName("Every signature sign() gives still verifies off the card, for keys of both parities and getW in either form (65 and 33 bytes), and was checked on the card first: one key agreement for R and one for [e]Q, a third ([d]G) where getW gives 33 bytes, and one point addition for [s]G - T; out holds the 64 bytes and nothing past them")
    void everySignatureIsCheckedAndStillVerifies() throws Exception {
        for (boolean compressed : new boolean[] { false, true }) {
            for (Object[] k : sigKEYS) {
                Object[] c = countedSigner();
                SchnorrHW s = (SchnorrHW) c[0];
                Counted ecdh = (Counted) c[1], gm = (Counted) c[2];
                byte[] w = hexToBytes((String) k[1]);
                byte[] m = msg(w[3]), out = untouched();
                ChipPublicKey pub = new ChipPublicKey(sigXy(w, 0), compressed);
                assertEquals(64, s.sign(sigKeyOf((String) k[0]), pub, m, (short) 0, out, (short) 8));
                assertTrue(schnorrVerify(xOf(w), m, Arrays.copyOfRange(out, 8, 72)), "verifies: d = " + k[0] + (compressed ? ", getW 33" : ", getW 65"));
                assertArrayEquals(Arrays.copyOf(untouched(), 8), Arrays.copyOf(out, 8), "nothing before it");
                assertArrayEquals(Arrays.copyOfRange(untouched(), 72, 96), Arrays.copyOfRange(out, 72, 96), "nothing after it");
                assertEquals(compressed ? 3 : 2, ecdh.answers.size(), "the key agreement: R, [e]Q" + (compressed ? " and [d]G" : ""));
                assertEquals(1, gm.answers.size(), "the point addition: [s]G - T");
                // the check's R' is R, and the T it added is [e]Q negated
                byte[] r = ecdh.answers.get(0), rCheck = gm.answers.get(0);
                assertArrayEquals(Arrays.copyOfRange(out, 8, 40), xOf(rCheck), "R' has R's x");
                assertFalse(new BigInteger(1, Arrays.copyOfRange(rCheck, 33, 65)).testBit(0), "and an even Y");
                assertArrayEquals(xOf(r), xOf(rCheck), "the R of the nonce, whatever its parity");
                leftNothing(s, "after a signature");
            }
        }
    }

    // =========================================================================
    // (b) a key that does not agree: refused, nothing out
    // =========================================================================

    @Test
    @DisplayName("sign() refuses 6F00 a public key object that names another key than the private one, getW of 65 bytes or of 33, and a private key one bit off its public key (a bit of d flipped): the caller's buffer is as it was, byte for byte, the scratch and the work are zero, the key holds G's x, and the signer signs after it")
    void aKeyThatDoesNotAgreeIsRefused() throws Exception {
        SchnorrHW s = signer();
        for (boolean compressed : new boolean[] { false, true }) {
            for (int i = 0; i < sigKEYS.length; i++) {
                final int at = i;
                Object[] mine = sigKEYS[i], other = sigKEYS[(i + 1) % sigKEYS.length];
                ECPublicKey wrong = new ChipPublicKey(sigXy(hexToBytes((String) other[1]), 0), compressed);
                byte[] out = untouched();
                String what = "d = " + mine[0] + " under the key of " + other[0] + (compressed ? ", getW 33" : ", getW 65");
                assertEquals(Secp256k1.SW_CRYPTO_ERROR, refusal(() -> s.sign(sigKeyOf((String) mine[0]), wrong, msg(at), (short) 0, out, (short) 8)), what);
                assertArrayEquals(untouched(), out, what + ": out untouched");
                leftNothing(s, what);
                sigLeftAtG(s, what);
            }
            // d one bit off: the private key's scalar flipped in its lowest bit, the public key the true one
            for (Object[] k : sigKEYS) {
                BigInteger d = new BigInteger((String) k[0], 16).flipBit(0);
                ECPublicKey pub = new ChipPublicKey(sigXy(hexToBytes((String) k[1]), 0), compressed);
                byte[] out = untouched();
                String what = "d = " + k[0] + " with its lowest bit flipped" + (compressed ? ", getW 33" : ", getW 65");
                assertEquals(Secp256k1.SW_CRYPTO_ERROR, refusal(() -> s.sign(sigKeyOf(toHex(sigU32(d))), pub, msg(5), (short) 0, out, (short) 0)), what);
                assertArrayEquals(untouched(), out, what + ": out untouched");
                leftNothing(s, what);
            }
        }
        byte[] w = hexToBytes((String) sigKEYS[4][1]), out = new byte[64];
        s.sign(sigKeyOf((String) sigKEYS[4][0]), sigPubOf((String) sigKEYS[4][1]), msg(9), (short) 0, out, (short) 0);
        assertTrue(schnorrVerify(xOf(w), msg(9), out), "and it signs after them");
    }

    @Test
    @DisplayName("A getW of 33 bytes is made whole by [d]G with the card's private key in the key agreement, never by a square root: the key agreement is asked for G under the private key; a getW of 65 bytes that does not begin 04, or of another length, is refused 6F00 before any signature is made")
    void aShortGetWIsMadeWholeByTheKeyAgreement() throws Exception {
        Object[] c = countedSigner();
        SchnorrHW s = (SchnorrHW) c[0];
        Counted ecdh = (Counted) c[1];
        Object[] k = sigKEYS[3];
        byte[] w = hexToBytes((String) k[1]);
        s.sign(sigKeyOf((String) k[0]), new ChipPublicKey(sigXy(w, 0), true), msg(1), (short) 0, new byte[64], (short) 0);
        assertTrue(ecdh.answers.stream().anyMatch(a -> Arrays.equals(a, w)), "[d]G, the whole point, came of the key agreement");
        for (byte lead : new byte[] { 0x00, 0x06 }) {
            ChipPublicKey odd = new ChipPublicKey(sigXy(w, 0), false);
            odd.w65[0] = lead;
            byte[] out = untouched();
            assertEquals(Secp256k1.SW_CRYPTO_ERROR, refusal(() -> s.sign(sigKeyOf((String) k[0]), odd, msg(2), (short) 0, out, (short) 0)), "a lead byte of " + lead);
            assertArrayEquals(untouched(), out);
        }
    }

    // =========================================================================
    // (c) and (d) a date's key (D197)
    // =========================================================================

    /** t, then the 33-byte P + t*G: what a payment's `group` holds for the signer. */
    static byte[] dateKey(BigInteger d, BigInteger t, BigInteger tPoint) {
        return concat(sigU32(t), compressed(times(d.add(tPoint))));
    }

    @Test
    @DisplayName("Under a date's key (D197) sign() refuses 6F00, out untouched, a point at g + 32 that is not P + t*G (P + (t + 1)*G), and P + t*G with its parity byte the other one; the true point signs and verifies under x(P + t*G); getW of 65 bytes and of 33")
    void aDatesPointMustBePPlusTG() throws Exception {
        SchnorrHW s = signer();
        java.util.Random r = new java.util.Random(197);
        for (boolean compressed : new boolean[] { false, true }) {
            for (Object[] k : sigKEYS) {
                BigInteger d = new BigInteger((String) k[0], 16), t = new BigInteger(255, r).add(BigInteger.ONE);
                ECPublicKey pub = new ChipPublicKey(sigXy(hexToBytes((String) k[1]), 0), compressed);
                ECPrivateKey priv = sigKeyOf((String) k[0]);
                byte[] good = dateKey(d, t, t), m = msg(t.intValue()), sig = new byte[64];
                s.sign(priv, pub, good, (short) 0, m, (short) 0, sig, (short) 0);
                assertTrue(schnorrVerify(Arrays.copyOfRange(good, 33, 65), m, sig), "verifies under P + t*G: d = " + k[0]);
                byte[] notIt = dateKey(d, t, t.add(BigInteger.ONE)), flipped = good.clone();
                flipped[32] ^= 1;
                for (byte[] g : new byte[][] { notIt, flipped }) {
                    byte[] out = untouched();
                    String what = (g == notIt ? "P + (t + 1)*G" : "P + t*G with the other parity") + ", d = " + k[0] + (compressed ? ", getW 33" : ", getW 65");
                    assertEquals(Secp256k1.SW_CRYPTO_ERROR, refusal(() -> s.sign(priv, pub, g, (short) 0, m, (short) 0, out, (short) 0)), what);
                    assertArrayEquals(untouched(), out, what + ": out untouched");
                    leftNothing(s, what);
                }
            }
        }
    }

    @Test
    @DisplayName("A date's key whose P + t*G has an odd Y, signed until the nonce's R has had an odd Y as well as an even one: every signature verifies under x(P + t*G), the check lifting both to an even Y")
    void anOddDateKeyAndAnOddRStillVerify() throws Exception {
        Object[] c = countedSigner();
        SchnorrHW s = (SchnorrHW) c[0];
        Counted ecdh = (Counted) c[1];
        Object[] k = sigKEYS[2];
        BigInteger d = new BigInteger((String) k[0], 16), t = BigInteger.ONE;
        while (!times(d.add(t))[1].testBit(0)) t = t.add(BigInteger.ONE);
        byte[] g = dateKey(d, t, t);
        assertEquals(3, g[32], "P + t*G has an odd Y");
        ECPublicKey pub = sigPubOf((String) k[1]);
        boolean oddR = false, evenR = false;
        for (int i = 0; i < 64 && !(oddR && evenR); i++) {
            ecdh.answers.clear();
            byte[] m = msg(i), sig = new byte[64];
            s.sign(sigKeyOf((String) k[0]), pub, g, (short) 0, m, (short) 0, sig, (short) 0);
            assertTrue(schnorrVerify(Arrays.copyOfRange(g, 33, 65), m, sig), "signature " + i);
            boolean odd = (ecdh.answers.get(0)[64] & 1) == 1;
            oddR |= odd;
            evenR |= !odd;
        }
        assertTrue(oddR && evenR, "both parities of R were met");
    }

    @Test
    @DisplayName("In the applet every signature is sign(), the waits too (work: a payment's, the PIN's, a held tap's, cleared after it, K1), and there is no unchecked signature: AUTH, a payment's (SIG_ALL), the settlement record's descriptor, the PIN key's signature and the waits' work")
    void theAppletSignsEverythingWithTheCheck() throws Exception {
        String code = appletCode();
        assertFalse(code.contains("signatureWork"), "no unchecked signature in the applet");
        for (String site : new String[] {
                "schnorr.sign(privs[s], pubs[s], x, X_HASH, buf, (short) 0);" }) {
            assertEquals(1, code.split(java.util.regex.Pattern.quote(site), -1).length - 1, site);
        }
        assertEquals(1, code.split("schnorr\\.sign\\(", -1).length - 1, "the signature that leaves the card is sign()");
    }

    @Test
    @DisplayName("D218 (the join, M2 open): on the chip's path a fault in e*d that makes a signature the check refuses never reaches out: the whole signature is made again by the bytecode, from step 1 with a fresh nonce (a second R), checked again, and verifies; the chip's path is given up for that signature only (D228: chipMul stays true); a key that does not agree is 6F00 even so, after the second try, out as it was and scratch and work zero")
    void onTheChipsPathARefusedSignatureIsMadeAgain() throws Exception {
        java.math.BigInteger d = java.math.BigInteger.valueOf(0x5EED5L).shiftLeft(130).add(java.math.BigInteger.valueOf(77));
        BigInteger[] pt = ecMulTest(d, SECP_GX, SECP_GY);
        byte[] m = msg(9);
        // a fault in the third squaring's output: the product is wrong, and so s
        SignHardeningTest.Rec eng = new SignHardeningTest.Rec();
        SchnorrHW s = SignHardeningTest.chipSigner(eng);
        Counted ecdh = new Counted(KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
        sigSetHidden(s, "ecdh", ecdh);
        eng.faultAt = 2; eng.flipOutBit = 17;
        byte[] out = untouched();
        assertEquals(64, s.sign(SignHardeningTest.ecPriv(ChipEnginesTest.be(d)), SignHardeningTest.ecPub(point65(pt)), m, (short) 0, out, (short) 8));
        assertTrue(schnorrVerify(ChipEnginesTest.be(pt[0]), m, Arrays.copyOfRange(out, 8, 72)), "the signature that leaves is the bytecode's, and verifies");
        assertTrue(ChipEnginesTest.flag(s, SchnorrHW.class, "chipMul"), "the chip's path is given up for that signature only (D228)");
        assertEquals(4, ecdh.answers.size(), "R and [e]Q of the refused try, then R and [e]Q again");
        assertFalse(Arrays.equals(xOf(ecdh.answers.get(0)), xOf(ecdh.answers.get(2))), "a fresh nonce: the second R is another");
        assertArrayEquals(xOf(ecdh.answers.get(2)), Arrays.copyOfRange(out, 8, 40), "and the signature is the second's");
        leftNothing(s, "after a signature made again");
        // a key that does not agree: refused on the chip's path, made again, refused again: 6F00
        SchnorrHW t = SignHardeningTest.chipSigner(new SignHardeningTest.Rec());
        BigInteger[] other = ecMulTest(d.add(java.math.BigInteger.ONE), SECP_GX, SECP_GY);
        byte[] refused = untouched();
        assertEquals(Secp256k1.SW_CRYPTO_ERROR & 0xFFFF,
            refusal(() -> t.sign(SignHardeningTest.ecPriv(ChipEnginesTest.be(d)), SignHardeningTest.ecPub(point65(other)), m, (short) 0, refused, (short) 0)));
        assertArrayEquals(untouched(), refused, "nothing written");
        assertTrue(ChipEnginesTest.flag(t, SchnorrHW.class, "chipMul"), "kept for the next signature (D228)");
        leftNothing(t, "after a refusal on the chip's path");
    }

    // =========================================================================
    // The other project's helpers, as this file uses them
    // =========================================================================

    static String appletCode() throws Exception {
        String src = new String(java.nio.file.Files.readAllBytes(
            SchnorrHWMathTest.mainSourceDir().resolve("FedimintCardApplet.java")), java.nio.charset.StandardCharsets.UTF_8);
        return SchnorrHWMathTest.stripCommentsAndCharLiterals(src);
    }

    /** { private key d, uncompressed public key 04||X||Y, "is P.y odd" }. */
    static final Object[][] sigKEYS = {
        { "0000000000000000000000000000000000000000000000000000000000000003",
          "04F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9"
          + "388F7B0F632DE8140FE337E62A37F3566500A99934C2231B6CB9FD7584B8E672",
          Boolean.FALSE },
        { "0000000000000000000000000000000000000000000000000000000000000002",
          "04C6047F9441ED7D6D3045406E95C07CD85C778E4B8CEF3CA7ABAC09B95C709EE5"
          + "1AE168FEA63DC339A3C58419466CEAEEF7F632653266D0E1236431A950CFE52A",
          Boolean.FALSE },
        { "B7E151628AED2A6ABF7158809CF4F3C762E7160F38B4DA56A784D9045190CFEF",
          "04DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659"
          + "2CE19B946C4EE58546F5251D441A065EA50735606985E5B228788BEC4E582898",
          Boolean.FALSE },
        { "0000000000000000000000000000000000000000000000000000000000000006",
          "04FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A1460297556"
          + "AE12777AACFBB620F3BE96017F45C560DE80F0F6518FE4A03C870C36B075F297",
          Boolean.TRUE },
        { "0000000000000000000000000000000000000000000000000000000000000009",
          "04ACD484E2F0C7F65309AD178A9F559ABDE09796974C57E714C35F110DFC27CCBE"
          + "CC338921B0A7D9FD64380971763B61E9ADD888A4375F8E0F05CC262AC64F9C37",
          Boolean.TRUE },
        { "0B432B2677937381AEF05BB02A66ECD012773062CF3FA2549E44F58ED2401710",
          "0425D1DFF95105F5253C4022F628A996AD3A0D95FBF21D468A1B33F8C160D8F517"
          + "0CFEB7AC4341CB6441C702568A8C0FBDC873B0CF5C8181FDAFE3AEE6F49CD4A9",
          Boolean.TRUE },
    };

    static Object sigHidden(SchnorrHW s, String name) throws Exception {
        java.lang.reflect.Field f = SchnorrHW.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(s);
    }

    static void sigSetHidden(SchnorrHW s, String name, Object value) throws Exception {
        java.lang.reflect.Field f = SchnorrHW.class.getDeclaredField(name);
        f.setAccessible(true);
        f.set(s, value);
    }

    static ECPrivateKey sigKeyOf(String hexD) {
        ECPrivateKey priv = (ECPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
        ECPublicKey pub = (ECPublicKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        Secp256k1.curve(priv);
        Secp256k1.curve(pub);
        priv.setS(hexToBytes(hexD), (short) 0, (short) 32);
        return priv;
    }

    static ECPublicKey sigPubOf(String hexW) {
        ECPrivateKey priv = (ECPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
        ECPublicKey pub = (ECPublicKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        Secp256k1.curve(priv);
        Secp256k1.curve(pub);
        pub.setW(hexToBytes(hexW), (short) 0, (short) 65);
        return pub;
    }

    static boolean sigAllZero(byte[] b) {
        for (byte x : b) if (x != 0) return false;
        return true;
    }

    static byte[] sigU32(BigInteger v) {
        byte[] raw = v.toByteArray(), out = new byte[32];
        int len = Math.min(raw.length, 32);
        System.arraycopy(raw, raw.length - len, out, 32 - len, len);
        return out;
    }

    static byte[] sigPoint(BigInteger x, BigInteger y) {
        byte[] out = new byte[65];
        out[0] = 4;
        System.arraycopy(sigU32(x), 0, out, 1, 32);
        System.arraycopy(sigU32(y), 0, out, 33, 32);
        return out;
    }

    /** X and Y of the 65-byte point at b[off]. */
    static BigInteger[] sigXy(byte[] b, int off) {
        return new BigInteger[] { new BigInteger(1, java.util.Arrays.copyOfRange(b, off + 1, off + 33)), new BigInteger(1, java.util.Arrays.copyOfRange(b, off + 33, off + 65)) };
    }

    /** The key's generator is G and its scalar G's x (or a key in RAM holds nothing) after the check. */
    static void sigLeftAtG(SchnorrHW schnorr, String what) throws Exception {
        ECPrivateKey tmp = (ECPrivateKey) sigHidden(schnorr, "tmpPriv");
        if (!tmp.isInitialized()) return;
        byte[] g = new byte[65], k = new byte[32];
        tmp.getG(g, (short) 0);
        tmp.getS(k, (short) 0);
        org.junit.jupiter.api.Assertions.assertArrayEquals(Secp256k1.SECP256K1_G, g, what + ": the key's generator is G after the check");
        org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(Secp256k1.SECP256K1_G, 1, 33), k, what + ": G's x left in the key");
    }
}
