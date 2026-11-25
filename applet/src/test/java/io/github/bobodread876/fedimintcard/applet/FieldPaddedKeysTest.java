package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.DEFAULT_PIN;
import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.bobodread876.fedimintcard.applet.ChipEnginesTest.Engine;
import io.github.bobodread876.fedimintcard.applet.ChipEnginesTest.FakeKey;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.math.BigInteger;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Random;
import javacard.framework.ISOException;
import javacard.security.RSAPrivateKey;
import javacardx.crypto.Cipher;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

/**
 * The curve check's field arithmetic on both of its paths: the chip's RSA mod p*p with the bytecode reduction after, and
 * the same three powers over p itself padded to 64 bytes (fieldP), whose answers are under p at once. Ported from Cashu
 * Card's applet tests (FieldReductionTest: reduceModPMatchesBigInteger, smallHelpersMatchBigInteger; ChipEnginesTest:
 * theSimulatorKeepsTheOldPaths, thePaddedKeysDoTheField, aFieldEngineThatFailsIsNotUsed,
 * aValueAtOrOverPIsNeverGivenToThePaddedKeys, paddedPowersAreRight) and adapted to this applet, whose field keys are
 * built and held to their known answers at ATTEST_INIT. jCardSim refuses a padded modulus ("not composite"), so the
 * padded path runs on ChipEnginesTest's Engine, which computes with BigInteger what the chip's RSA does and answers
 * garbage for an input at or over the modulus, as the chip does.
 */
class FieldPaddedKeysTest {
    static final BigInteger P = new BigInteger("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F", 16);
    static final BigInteger ROOT_EXP = P.add(BigInteger.ONE).shiftRight(2).add(P.subtract(BigInteger.ONE).shiftRight(1));
    static final BigInteger EULER_EXP = P.subtract(BigInteger.ONE).shiftRight(1);
    static final BigInteger GX = new BigInteger("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798", 16);
    static final BigInteger GY = new BigInteger("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8", 16);
    static final int ATTEST_INIT = 0x30;
    static final int AT = 5;            // the point where a sealed command's E is: OFFSET_CDATA
    static final int FIELD = 112, FIELD_END = 240;

    static byte[] fixed(BigInteger v, int len) {
        byte[] raw = v.toByteArray(), out = new byte[len];
        int n = Math.min(raw.length, len);
        System.arraycopy(raw, raw.length - n, out, len - n, n);
        return out;
    }

    static Method method(String name, Class<?>... types) throws Exception {
        Method m = FedimintCardApplet.class.getDeclaredMethod(name, types);
        m.setAccessible(true);
        return m;
    }

    static Object appletOf(CardTestSupport c) { return c.runtime.appletAt(c.aid); }

    /** A card whose chip RSA is `e`: the card's real RSA beside fake padded keys, put in before the first ATTEST_INIT. */
    static CardTestSupport engineCard(Engine[] out) throws Exception {
        CardTestSupport c = new CardTestSupport();
        Engine e = new Engine(Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false));
        out[0] = e;
        c.setField("rsa", e);
        c.setField("rsaCubeP", new FakeKey());
        c.setField("rsaRootP", new FakeKey());
        c.setField("rsaEulerP", new FakeKey());
        return c;
    }

    // ---- the reduction and the small helpers (the p*p path) ---------------------------------------------------------

    @Test
    @DisplayName("reduceModP: a 512-bit number mod p, in its low 32 bytes with the high 32 cleared, for the edges (0, 1, p - 1, p, p + 1, 2p, p*p - 1, 2^512 - 1, ...) and three hundred random numbers; nothing outside the number and its forty bytes of room is touched, and the room is left zero")
    void reduceModPMatchesBigInteger() throws Exception {
        Method m = method("reduceModP", byte[].class, short.class, byte[].class, short.class);
        SecureRandom rnd = new SecureRandom();
        BigInteger pp = P.multiply(P);
        BigInteger[] edges = { BigInteger.ZERO, BigInteger.ONE, P.subtract(BigInteger.ONE), P, P.add(BigInteger.ONE), P.shiftLeft(1),
            pp.subtract(BigInteger.ONE), pp.subtract(P), P.shiftLeft(256), P.shiftLeft(256).add(P), BigInteger.ONE.shiftLeft(512).subtract(BigInteger.ONE),
            BigInteger.ONE.shiftLeft(256), BigInteger.ONE.shiftLeft(256).subtract(BigInteger.ONE) };
        for (int i = 0; i < edges.length + 300; i++) {
            BigInteger v = i < edges.length ? edges[i] : new BigInteger(512, rnd);
            byte[] buf = new byte[300], work = new byte[200];
            System.arraycopy(fixed(v, 64), 0, buf, 96, 64);
            m.invoke(null, buf, (short) 96, work, (short) 120);
            assertEquals(v.mod(P), new BigInteger(1, Arrays.copyOfRange(buf, 128, 160)), "v = " + v.toString(16));
            for (int k = 96; k < 128; k++) assertEquals(0, buf[k], "the high half is cleared, v = " + v.toString(16));
            for (int k = 0; k < 96; k++) assertEquals(0, buf[k], "nothing before the number is touched");
            for (int k = 160; k < 300; k++) assertEquals(0, buf[k], "nor after it");
            for (int k = 0; k < 200; k++) assertEquals(0, work[k], "the working room is the forty bytes asked for, zeroed after");
        }
    }

    @Test
    @DisplayName("add7ModP and negateModP: v + 7 mod p and p - v, for the edges (0, 1, 6, p - 8, p - 7, p - 6, p - 1) and a hundred random numbers under p")
    void smallHelpersMatchBigInteger() throws Exception {
        Method add7 = method("add7ModP", byte[].class, short.class);
        Method neg = method("negateModP", byte[].class, short.class);
        SecureRandom rnd = new SecureRandom();
        BigInteger[] edges = { BigInteger.ZERO, BigInteger.ONE, BigInteger.valueOf(6), P.subtract(BigInteger.valueOf(8)), P.subtract(BigInteger.valueOf(7)),
            P.subtract(BigInteger.valueOf(6)), P.subtract(BigInteger.ONE) };
        for (int i = 0; i < edges.length + 100; i++) {
            BigInteger v = i < edges.length ? edges[i] : new BigInteger(256, rnd).mod(P);
            byte[] buf = new byte[64];
            System.arraycopy(fixed(v, 32), 0, buf, 16, 32);
            add7.invoke(null, buf, (short) 16);
            assertEquals(v.add(BigInteger.valueOf(7)).mod(P), new BigInteger(1, Arrays.copyOfRange(buf, 16, 48)), "v + 7, v = " + v.toString(16));
            if (v.signum() == 0) continue;
            System.arraycopy(fixed(v, 32), 0, buf, 16, 32);
            neg.invoke(null, buf, (short) 16);
            assertEquals(P.subtract(v), new BigInteger(1, Arrays.copyOfRange(buf, 16, 48)), "p - v, v = " + v.toString(16));
        }
    }

    // ---- which path a card takes --------------------------------------------------------------------------------------

    @Test
    @DisplayName("the simulator refuses the padded keys: fieldP is false before ATTEST_INIT and stays false after it, the ATTEST_INIT succeeds all the same, and a sealed right PIN verifies on the p*p keys")
    void theSimulatorKeepsThePpPath() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertFalse((Boolean) c.field("fieldP"), "nothing known before the first ATTEST_INIT");
        assertSw(0x9000, c.tx(ATTEST_INIT, 0, null));
        assertFalse((Boolean) c.field("fieldP"), "the simulator refuses the padded modulus: the p*p keys stay");
        c.setPin(DEFAULT_PIN);
        c.select();
        c.verifyPin(DEFAULT_PIN);
        assertSw(0x63C1, c.verifyPinRaw(new byte[] { '9', '9', '9', '9' }), "and a wrong one is a try");
    }

    @Test
    @DisplayName("on an engine that holds, ATTEST_INIT sets the three padded keys (modulus p, 32 zero bytes before it, 64 in all; the exponents of the cube, Euler's test and the root), runs their known answers and sets fieldP; a sealed VERIFY then holds E to the curve by the padded keys alone, never once by the p*p keys, never with an input at or over p, and the right PIN verifies and a wrong one is a try")
    void thePaddedKeysDoTheField() throws Exception {
        Engine[] e = new Engine[1];
        CardTestSupport c = engineCard(e);
        assertSw(0x9000, c.tx(ATTEST_INIT, 0, null));
        assertTrue((Boolean) c.field("fieldP"), "the known answers held");
        for (BigInteger m : e[0].moduli) assertEquals(P, m, "every padded power so far is mod p");
        assertEquals(11, e[0].powers, "the known answers: three small, three at full width, five at the top of the range");
        for (String n : new String[] { "rsaCubeP", "rsaRootP", "rsaEulerP" }) {
            FakeKey k = (FakeKey) c.field(n);
            assertEquals(P, k.mod, n + "'s modulus is p");
            assertEquals(64, k.modLen, n + "'s modulus is 64 bytes: p padded with zeros before it");
        }
        assertEquals(BigInteger.valueOf(3), ((FakeKey) c.field("rsaCubeP")).exp);
        assertEquals(EULER_EXP, ((FakeKey) c.field("rsaEulerP")).exp);
        assertEquals(ROOT_EXP, ((FakeKey) c.field("rsaRootP")).exp, "the root's exponent is the p*p key's");
        c.setPin(DEFAULT_PIN);
        assertTrue((Boolean) c.field("fieldP"), "ATTEST_INIT again (the certificate's) holds again");
        c.select();
        e[0].powers = 0;
        e[0].oldPowers = 0;
        e[0].garbage = 0;
        c.verifyPin(DEFAULT_PIN);
        assertEquals(3, e[0].powers, "the cube, Euler's test and the root of E, by the padded keys");
        assertEquals(0, e[0].oldPowers, "not one power of the p*p keys");
        assertEquals(0, e[0].garbage, "and not one input at or over the modulus");
        assertSw(0x63C1, c.verifyPinRaw(new byte[] { '9', '9', '9', '9' }), "a wrong PIN is still a try");
        assertEquals(0, e[0].oldPowers);
    }

    @Test
    @DisplayName("where the padded keys fail their known answers (an engine that answers wrong, one that refuses every padded power, one right on small values but wrong on a full-width one), fieldP stays false, ATTEST_INIT succeeds, and a sealed right PIN verifies on the p*p keys")
    void aFieldEngineThatFailsIsNotUsed() throws Exception {
        for (String how : new String[] { "lie", "refuse", "wide" }) {
            Engine[] e = new Engine[1];
            CardTestSupport c = engineCard(e);
            if (how.equals("lie")) e[0].lie = true;
            if (how.equals("refuse")) e[0].refuse = true;
            if (how.equals("wide")) {
                // right on small values, wrong above 2^128: a chip whose engine drops the high bytes
                Engine w = new Engine(Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false)) {
                    @Override public short doFinal(byte[] a, short o, short l, byte[] out, short oo) {
                        short n = super.doFinal(a, o, l, out, oo);
                        if (key != null && !isZero16(out, oo + 32)) Arrays.fill(out, oo + 32, oo + 48, (byte) 0);
                        return n;
                    }
                    boolean isZero16(byte[] b, int off) { for (int i = 0; i < 16; i++) if (b[off + i] != 0) return false; return true; }
                };
                e[0] = w;
                c.setField("rsa", w);
            }
            assertSw(0x9000, c.tx(ATTEST_INIT, 0, null), how);
            assertFalse((Boolean) c.field("fieldP"), how + ": the padded keys are not leaned on");
            c.setPin(DEFAULT_PIN);
            c.select();
            int before = e[0].powers;
            c.verifyPin(DEFAULT_PIN);
            assertEquals(before, e[0].powers, how + ": no padded power in the VERIFY");
            assertTrue(e[0].oldPowers >= 3, how + ": the p*p keys held E to the curve");
        }
    }

    @Test
    @DisplayName("a padded power of an x at or over p (p, p + 1, 2^256 - 1) is refused (6F00) with nothing asked of the engine, which would answer garbage; p - 1 cubed is p - 1")
    void aValueAtOrOverPIsNeverGivenToThePaddedKeys() throws Exception {
        Engine[] e = new Engine[1];
        CardTestSupport c = engineCard(e);
        assertSw(0x9000, c.tx(ATTEST_INIT, 0, null));
        assertTrue((Boolean) c.field("fieldP"));
        Method m = method("powModP", RSAPrivateKey.class, byte[].class, short.class);
        Object cube = c.field("rsaCube");
        for (BigInteger v : new BigInteger[] { P, P.add(BigInteger.ONE), BigInteger.ONE.shiftLeft(256).subtract(BigInteger.ONE) }) {
            byte[] a = new byte[128];
            System.arraycopy(fixed(v, 32), 0, a, 32, 32);
            int before = e[0].powers;
            InvocationTargetException t = assertThrows(InvocationTargetException.class, () -> m.invoke(appletOf(c), cube, a, (short) 0));
            assertTrue(t.getCause() instanceof ISOException, "refused: " + t.getCause());
            assertEquals(Secp256k1.SW_CRYPTO_ERROR, ((ISOException) t.getCause()).getReason());
            assertEquals(before, e[0].powers, "no power was asked of the engine");
        }
        byte[] a = new byte[128];
        System.arraycopy(fixed(P.subtract(BigInteger.ONE), 32), 0, a, 32, 32);
        m.invoke(appletOf(c), cube, a, (short) 0);
        assertArrayEquals(fixed(P.subtract(BigInteger.ONE), 32), Arrays.copyOfRange(a, 96, 128));
    }

    @Test
    @DisplayName("the powers on both paths against BigInteger: 25 values (0, 1, 2, p - 1, p - 2, G's x and y, the half of p and one over it, and random ones) each cubed, raised to (p - 1)/2 and to the root's exponent, by the padded keys (under p at once, the high half zero) and by the p*p keys with the reduction, the same answers")
    void paddedPowersAreRight() throws Exception {
        Engine[] e = new Engine[1];
        CardTestSupport padded = engineCard(e);
        assertSw(0x9000, padded.tx(ATTEST_INIT, 0, null));
        assertTrue((Boolean) padded.field("fieldP"));
        CardTestSupport pp = new CardTestSupport();
        assertSw(0x9000, pp.tx(ATTEST_INIT, 0, null));
        assertFalse((Boolean) pp.field("fieldP"));
        Method m = method("powModP", RSAPrivateKey.class, byte[].class, short.class);
        Random rnd = new Random(2311);
        List<BigInteger> xs = new ArrayList<>(Arrays.asList(BigInteger.ZERO, BigInteger.ONE, BigInteger.TWO, P.subtract(BigInteger.ONE), P.subtract(BigInteger.TWO), GX, GY, P.shiftRight(1), P.shiftRight(1).add(BigInteger.ONE)));
        while (xs.size() < 25) xs.add(new BigInteger(256, rnd).mod(P));
        String[] keys = { "rsaCube", "rsaEuler", "rsaRoot" };
        BigInteger[] exps = { BigInteger.valueOf(3), EULER_EXP, ROOT_EXP };
        int n = 0;
        for (BigInteger v : xs) {
            for (int k = 0; k < 3; k++) {
                BigInteger want = v.modPow(exps[k], P);
                for (CardTestSupport c : new CardTestSupport[] { padded, pp }) {
                    byte[] a = new byte[128];
                    System.arraycopy(fixed(v, 32), 0, a, 32, 32);
                    m.invoke(appletOf(c), c.field(keys[k]), a, (short) 0);
                    String path = c == padded ? "padded" : "p*p";
                    assertEquals(want, new BigInteger(1, Arrays.copyOfRange(a, 96, 128)), path + " " + keys[k] + " of " + v.toString(16));
                    assertArrayEquals(new byte[32], Arrays.copyOfRange(a, 64, 96), path + ": the answer's high half is zero");
                }
                n++;
            }
        }
        assertEquals(75, n);
        assertEquals(0, e[0].garbage);
    }

    // ---- the curve check itself ---------------------------------------------------------------------------------------

    /** The 04 form of (x, y), at AT in a 261-byte APDU buffer. */
    static byte[] point(int prefix, BigInteger x, BigInteger y) {
        byte[] a = new byte[261];
        a[AT] = (byte) prefix;
        System.arraycopy(fixed(x, 32), 0, a, AT + 1, 32);
        System.arraycopy(fixed(y, 32), 0, a, AT + 33, 32);
        return a;
    }

    static boolean onCurve(BigInteger x, BigInteger y) {
        return x.compareTo(P) < 0 && y.compareTo(P) < 0 && y.modPow(BigInteger.TWO, P).equals(x.pow(3).add(BigInteger.valueOf(7)).mod(P));
    }

    /** Whether requireFullPoint takes the point; its room and the seal's are checked on the way. */
    static boolean takes(CardTestSupport c, byte[] a) throws Exception {
        Method m = method("requireFullPoint", byte[].class, short.class);
        byte[] before = a.clone();
        byte[] x = (byte[]) c.field("x");
        Arrays.fill(x, (byte) 0);
        x[98] = (byte) 0x40;
        boolean took;
        try {
            m.invoke(appletOf(c), a, (short) AT);
            took = true;
        } catch (InvocationTargetException t) {
            assertTrue(t.getCause() instanceof ISOException, "a refusal is an ISOException: " + t.getCause());
            assertEquals(0x6A80, ((ISOException) t.getCause()).getReason() & 0xFFFF);
            took = false;
        }
        for (int k = 0; k < a.length; k++) {
            if (k < FIELD || k >= FIELD_END) assertEquals(before[k], a[k], "byte " + k + " outside the field room is untouched");
        }
        assertEquals((byte) 0x40, x[98], "the INS byte of the seal's room is untouched");
        for (int k = 0; k < 98; k++) assertEquals(0, x[k], "nor the rest of the seal's room");
        for (int k = 100; k < x.length; k++) assertEquals(0, x[k], "and the reduction's room is left zero");
        return took;
    }

    @Test
    @DisplayName("requireFullPoint on both paths against BigInteger: G and -G taken; G with y + 1 and with y's top bit flipped, x = 5 (no root), a twist point, x and y at p and over it (p, p + 1, 2^256 - 1), the point at infinity's encodings (00 and zeros, 04 and zeros), the compressed and other prefixes, refused; for x = 0..7, p - 1, p - 2, G's x and 20 random x, both roots taken where x^3 + 7 is a square and refused where it is not, y + 1 refused; nothing outside the field room is written")
    void requireFullPointAgreesWithBigInteger() throws Exception {
        Engine[] e = new Engine[1];
        CardTestSupport padded = engineCard(e);
        assertSw(0x9000, padded.tx(ATTEST_INIT, 0, null));
        assertTrue((Boolean) padded.field("fieldP"));
        CardTestSupport pp = new CardTestSupport();
        assertSw(0x9000, pp.tx(ATTEST_INIT, 0, null));
        assertFalse((Boolean) pp.field("fieldP"));
        BigInteger two256 = BigInteger.ONE.shiftLeft(256);
        BigInteger twist = P.subtract(BigInteger.valueOf(132)).mod(P);   // y^2 = -(5^3 + 7): the twist's equation at x = 5
        BigInteger twistY = twist.modPow(EULER_EXP, P).equals(BigInteger.ONE) ? twist.modPow(P.add(BigInteger.ONE).shiftRight(2), P) : BigInteger.ONE;
        List<byte[]> refused = new ArrayList<>(List.of(
            point(4, GX, GY.add(BigInteger.ONE)),
            point(4, GX, GY.flipBit(255)),
            point(4, BigInteger.valueOf(5), GY),
            point(4, BigInteger.valueOf(5), twistY),
            point(4, P, GY), point(4, P.add(BigInteger.ONE), GY), point(4, two256.subtract(BigInteger.ONE), GY),
            point(4, GX, P), point(4, GX, P.add(GY)), point(4, GX, two256.subtract(BigInteger.ONE)),
            point(0, BigInteger.ZERO, BigInteger.ZERO), point(4, BigInteger.ZERO, BigInteger.ZERO),
            point(2, GX, GY), point(3, GX, GY), point(6, GX, GY), point(7, GX, GY)));
        List<byte[]> taken = new ArrayList<>(List.of(point(4, GX, GY), point(4, GX, P.subtract(GY))));
        List<BigInteger> xs = new ArrayList<>();
        for (int i = 0; i < 8; i++) xs.add(BigInteger.valueOf(i));
        xs.add(P.subtract(BigInteger.ONE));
        xs.add(P.subtract(BigInteger.TWO));
        xs.add(GX);
        Random rnd = new Random(1124);
        while (xs.size() < 31) xs.add(new BigInteger(256, rnd).mod(P));
        int squares = 0;
        for (BigInteger v : xs) {
            BigInteger s = v.pow(3).add(BigInteger.valueOf(7)).mod(P);
            if (s.modPow(EULER_EXP, P).equals(BigInteger.ONE)) {
                BigInteger y = s.modPow(P.add(BigInteger.ONE).shiftRight(2), P);
                assertTrue(onCurve(v, y) && onCurve(v, P.subtract(y)));
                taken.add(point(4, v, y));
                taken.add(point(4, v, P.subtract(y)));
                refused.add(point(4, v, y.add(BigInteger.ONE).mod(P)));
                squares++;
            } else {
                refused.add(point(4, v, BigInteger.ONE));
                refused.add(point(4, v, GY));
            }
        }
        assertTrue(squares > 5 && squares < xs.size(), "both kinds of x are asked");
        for (CardTestSupport c : new CardTestSupport[] { padded, pp }) {
            String path = c == padded ? "padded" : "p*p";
            for (byte[] a : taken) assertTrue(takes(c, a), path + " takes " + hex(a));
            for (byte[] a : refused) assertFalse(takes(c, a), path + " refuses " + hex(a));
        }
        assertEquals(0, e[0].garbage, "the padded keys were never given an input at or over p");
        assertNotNull(padded.field("rsaCubeP"));
    }

    static String hex(byte[] a) {
        StringBuilder b = new StringBuilder();
        for (int k = AT; k < AT + 65; k++) b.append(String.format("%02X", a[k] & 0xFF));
        return b.toString();
    }
}
