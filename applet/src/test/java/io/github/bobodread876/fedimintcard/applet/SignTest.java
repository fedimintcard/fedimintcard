package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.security.SecureRandom;
import java.util.Arrays;
import org.junit.jupiter.api.Test;

/**
 * BIP340 validity: every signature produced through the APDU interface is checked with
 * {@link Bip340#verify}, a pure-Java verifier pinned to the official vectors in
 * {@link Bip340VerifierVectorsTest}. This proves the signatures are valid BIP340, not that the
 * nonce derivation is the reference one (aux randomness is fresh per call).
 */
class SignTest {
    @Test
    void signReturns64BytesThatVerify() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        byte[] msg = CardTestSupport.fill(32, 0x42);
        byte[] sig = c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg);
        assertEquals(64, sig.length);
        assertTrue(Bip340.verify(pub, msg, sig));
    }

    @Test
    void edgeMessages() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        byte[] nMinus1 = Bip340.bytes32(Bip340.N.subtract(BigInteger.ONE));
        byte[] n = Bip340.bytes32(Bip340.N);
        byte[][] msgs = { new byte[32], CardTestSupport.fill(32, 0xFF), nMinus1, n };
        for (byte[] m : msgs) assertTrue(Bip340.verify(pub, m, c.signPin(CardTestSupport.DEFAULT_PIN, 0, m)), Bip340.hex(m));
    }

    @Test
    void signaturesVerifyOnlyUnderTheirOwnSlotKey() {
        CardTestSupport c = new CardTestSupport();
        byte[][] pubs = new byte[4][];
        for (int s = 0; s < 4; s++) pubs[s] = c.genKey(s);
        byte[] msg = CardTestSupport.fill(32, 7);
        for (int s = 0; s < 4; s++) {
            byte[] sig = c.signPin(CardTestSupport.DEFAULT_PIN, s, msg);
            for (int t = 0; t < 4; t++) {
                assertEquals(s == t, Bip340.verify(pubs[t], msg, sig), "sig " + s + " key " + t);
            }
        }
    }

    @Test
    void twentyRandomMessagesAcrossSlots() {
        CardTestSupport c = new CardTestSupport();
        byte[][] pubs = new byte[4][];
        for (int s = 0; s < 4; s++) pubs[s] = c.genKey(s);
        SecureRandom rnd = new SecureRandom();
        for (int i = 0; i < 20; i++) {
            byte[] msg = new byte[32];
            rnd.nextBytes(msg);
            int s = i % 4;
            assertTrue(Bip340.verify(pubs[s], msg, c.signPin(CardTestSupport.DEFAULT_PIN, s, msg)), "i=" + i);
        }
    }

    @Test
    void chosenKeysOfBothYParitiesSign() throws Exception {
        CardTestSupport c = new CardTestSupport();
        // Odd-y keys (6 and 9) exercise the d -> n-d branch; the others are even-y.
        String[] ds = { "03", "02", "06", "09",
            "B7E151628AED2A6ABF7158809CF4F3C762E7160F38B4DA56A784D9045190CFEF",
            "C90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B14E5C9",
            "0B432B2677937381AEF05BB02A66ECD012773062CF3FA2549E44F58ED2401710" };
        int odd = 0, even = 0;
        byte[] msg = Bip340.hex("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
        for (String h : ds) {
            BigInteger d = new BigInteger(h, 16);
            BigInteger[] pt = Bip340.pubkey(d);
            if (pt[1].testBit(0)) odd++; else even++;
            c.installKey(0, d);
            byte[] pubX = Bip340.bytes32(pt[0]);
            assertArrayEquals(pubX, c.getPub(0), "GET_PUB for d=" + h);
            assertTrue(Bip340.verify(pubX, msg, c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg)), "d=" + h);
        }
        assertTrue(odd >= 2 && even >= 2, "both parities covered: odd=" + odd + " even=" + even);
    }

    @Test
    void freshNonceSameMessageSignaturesDifferAndVerify() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        byte[] msg = CardTestSupport.fill(32, 1);
        byte[] a = c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg), b = c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg);
        assertFalse(Arrays.equals(a, b));
        assertTrue(Bip340.verify(pub, msg, a));
        assertTrue(Bip340.verify(pub, msg, b));
    }

    /** Anti-vacuity: the verifier rejects a corrupted card signature. */
    @Test
    void corruptedSignatureIsRejected() {
        CardTestSupport c = new CardTestSupport();
        byte[] pub = c.genKey(0);
        byte[] msg = CardTestSupport.fill(32, 9);
        byte[] sig = c.signPin(CardTestSupport.DEFAULT_PIN, 0, msg);
        assertTrue(Bip340.verify(pub, msg, sig));
        sig[63] ^= 1;
        assertFalse(Bip340.verify(pub, msg, sig));
        msg[0] ^= 1;
        assertFalse(Bip340.verify(pub, msg, c.signPin(CardTestSupport.DEFAULT_PIN, 0, CardTestSupport.fill(32, 9))));
    }

    /**
     * SIGN's own refusals, reached past the PIN gate: a blocked PIN answers 6983 to any SIGN, so
     * each card here is emptied to that state after its PIN is set (and verified where it matters).
     */
    @Test
    void emptySlotAndBadLengths() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        assertSw(0x6985, c.tx(CardTestSupport.SIGN, 0, new byte[32]));
        c.genKey(0);
        assertSw(0x6700, c.tx(CardTestSupport.SIGN, 0, new byte[31]));
        assertSw(0x6700, c.tx(CardTestSupport.SIGN, 0, new byte[33]));
        assertSw(0x6700, c.tx(CardTestSupport.SIGN, 0, null));
    }
}
