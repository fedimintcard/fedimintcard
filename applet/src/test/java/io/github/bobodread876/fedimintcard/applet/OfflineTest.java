package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.security.KeyFactory;
import java.security.PublicKey;
import java.security.spec.ECPoint;
import java.security.spec.ECPublicKeySpec;
import java.util.Arrays;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * Applet 3.0 (docs/OFFLINE-PLAN.md, F2): a slot signs one transaction, ever; notes, change and the
 * day's limit; the block-header clock; attestation.
 */
class OfflineTest {
    // Real mainnet headers: 900000, then 900200 (121 243 s later) and 900201 (553 s after that).
    static final byte[] H900000 = Bip340.hex("00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92");
    static final byte[] H900200 = Bip340.hex("0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104");
    static final byte[] H900201 = Bip340.hex("0000c020561cc99a795c39bfe4de51c4e1621a0643bef06c858900000000000000000000a60ddeb0320b09b52ddaf21ad7c492da26a49212e6e1848f22432b27ac2f8711575b4468743702177836e139");
    /** The genesis block: real, but with far less work than the floor. */
    static final byte[] GENESIS = Bip340.hex("0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c");

    static byte[] h(int v) { return fill(32, v); }

    static long u64(byte[] b, int off) {
        long v = 0;
        for (int i = 0; i < 8; i++) v = (v << 8) | (b[off + i] & 0xFF);
        return v;
    }

    static long u32(byte[] b, int off) {
        long v = 0;
        for (int i = 0; i < 4; i++) v = (v << 8) | (b[off + i] & 0xFF);
        return v;
    }

    /** A card with slots 0..n-1 keyed and holding notes of `msat`, a PIN set and verified. */
    static CardTestSupport card(int n, long msat, long limit) {
        CardTestSupport c = new CardTestSupport();
        if (limit != 0) assertSw(0x9000, c.tx(SET_LIMIT, 0, longBytes(limit)));
        for (int s = 0; s < n; s++) {
            c.genKey(s);
            assertSw(0x9000, c.tx(SET_NOTE, s, noteData(msat)));
        }
        c.setPin(DEFAULT_PIN);
        c.verifyPin(DEFAULT_PIN);
        return c;
    }

    static byte[] longBytes(long v) {
        byte[] b = new byte[8];
        for (int i = 0; i < 8; i++) b[7 - i] = (byte) (v >>> (8 * i));
        return b;
    }

    static boolean verifies(CardTestSupport c, int slot, byte[] msg, byte[] sig) {
        return Bip340.verify(c.getPub(slot), msg, sig);
    }

    // ── one transaction per slot ────────────────────────────────────────

    @Test
    void aSlotSignsOneTransactionAndOnlyThatOneAgain() {
        CardTestSupport c = card(1, 1000, 0);
        byte[] first = c.tx(SIGN, 0, h(1)).getData();
        assertEquals(64, first.length);
        assertTrue(verifies(c, 0, h(1), first));
        assertEquals(ST_SPENT, c.slotState(0));
        // the same transaction again: a lost answer, given back from storage, not signed again
        ResponseAPDU again = c.tx(SIGN, 0, h(1));
        assertSw(0x9000, again);
        assertArrayEquals(first, again.getData());
        // any other transaction is the same note spent twice
        assertSw(0x6985, c.tx(SIGN, 0, h(2)));
        // GET_SLOT shows what it spent on
        byte[] slot = c.tx(GET_SLOT, 0, null).getData();
        assertEquals(187, slot.length);
        assertEquals(ST_SPENT, slot[0]);
        assertArrayEquals(h(1), Arrays.copyOfRange(slot, 89, 121));
    }

    @Test
    void onlyANoteIsSigned() {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.genKey(1);
        assertSw(0x9000, c.tx(SET_CHANGE, 1, changeData(4096)));
        c.setPin(DEFAULT_PIN);
        c.verifyPin(DEFAULT_PIN);
        assertSw(0x6985, c.tx(SIGN, 0, h(1)), "a key with no note");
        assertSw(0x6985, c.tx(SIGN, 1, h(1)), "a key owed change");
        assertSw(0x6985, c.tx(SIGN, 2, h(1)), "an empty slot");
    }

    @Test
    void aKeyThatHoldsValueIsNeverReplaced() {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.genKey(1);
        c.genKey(2);
        assertSw(0x9000, c.tx(SET_NOTE, 0, noteData(1000)));
        assertSw(0x9000, c.tx(SET_CHANGE, 1, changeData(2048)));
        assertSw(0x6985, c.tx(CLA, GEN_KEY, 0, 1, null), "a note");
        assertSw(0x6985, c.tx(CLA, GEN_KEY, 1, 1, null), "change owed");
        // a KEYED slot (no value yet) may be re-keyed
        byte[] before = c.getPub(2);
        assertSw(0x9000, c.tx(CLA, GEN_KEY, 2, 1, null));
        assertFalse(Arrays.equals(before, c.getPub(2)));
    }

    @Test
    void aSpentSlotIsReKeyedAndForgetsItsTransaction() {
        CardTestSupport c = card(1, 1000, 0);
        assertSw(0x9000, c.tx(SIGN, 0, h(1)));
        byte[] before = c.getPub(0);
        assertSw(0x9000, c.tx(CLA, GEN_KEY, 0, 1, null));
        assertEquals(ST_KEYED, c.slotState(0));
        assertFalse(Arrays.equals(before, c.getPub(0)));
        byte[] slot = c.tx(GET_SLOT, 0, null).getData();
        assertArrayEquals(new byte[32], Arrays.copyOfRange(slot, 89, 121), "the spent hash is gone");
        assertSw(0x9000, c.tx(SET_NOTE, 0, noteData(1000)));
        assertSw(0x9000, c.tx(SIGN, 0, h(2)), "the new note signs its own transaction");
    }

    // ── notes and change ────────────────────────────────────────────────

    static byte[] changeData(long msat) {
        byte[] d = new byte[74];
        System.arraycopy(longBytes(msat), 0, d, 0, 8);
        for (int i = 8; i < 74; i++) d[i] = (byte) i;
        return d;
    }

    @Test
    void setNoteTakesOnlyAKeyedOrChangeSlot() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6985, c.tx(SET_NOTE, 0, noteData(1000)), "empty");
        c.genKey(0);
        assertSw(0x6A80, c.tx(SET_NOTE, 0, noteData(0)), "no amount");
        assertSw(0x6700, c.tx(SET_NOTE, 0, new byte[55]));
        assertSw(0x9000, c.tx(SET_NOTE, 0, noteData(1000)));
        assertSw(0x6985, c.tx(SET_NOTE, 0, noteData(2000)), "a note is not overwritten");
        byte[] slot = c.tx(GET_SLOT, 0, null).getData();
        assertEquals(ST_NOTE, slot[0]);
        assertArrayEquals(c.getPub(0), Arrays.copyOfRange(slot, 1, 33));
        assertArrayEquals(noteData(1000), Arrays.copyOfRange(slot, 33, 89));
    }

    @Test
    void changeIsKeptThenFinishedWithItsOwnAmount() {
        CardTestSupport c = new CardTestSupport();
        c.genKey(3);
        byte[] open = changeData(4096);
        assertSw(0x9000, c.tx(SET_CHANGE, 3, open));
        byte[] slot = c.tx(GET_SLOT, 3, null).getData();
        assertEquals(ST_CHANGE, slot[0]);
        assertEquals(4096, u64(slot, 33));
        assertArrayEquals(Arrays.copyOfRange(open, 8, 74), Arrays.copyOfRange(slot, 121, 187), "the opening");
        assertSw(0x6985, c.tx(SET_CHANGE, 3, open), "only a keyed slot");
        assertSw(0x6A80, c.tx(SET_NOTE, 3, noteData(2048)), "not the amount it is owed");
        assertSw(0x9000, c.tx(SET_NOTE, 3, noteData(4096)));
        slot = c.tx(GET_SLOT, 3, null).getData();
        assertEquals(ST_NOTE, slot[0]);
        assertArrayEquals(new byte[66], Arrays.copyOfRange(slot, 121, 187), "the opening is let go");
    }

    // ── the day's limit and the clock ───────────────────────────────────

    @Test
    void theLimitIsSetOnlyBeforeThePin() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_LIMIT, 0, longBytes(5000)));
        c.setPin(DEFAULT_PIN);
        c.verifyPin(DEFAULT_PIN);
        assertSw(0x6985, c.tx(SET_LIMIT, 0, longBytes(1L << 40)));
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(59, st.length);
        assertEquals(5000, u64(st, 40));
    }

    @Test
    void withNoHeaderTheLimitHoldsForGoodAndAnOverSpendBurnsNothing() {
        CardTestSupport c = card(3, 1000, 2000);
        assertSw(0x9000, c.tx(SIGN, 0, h(1)));
        assertSw(0x9000, c.tx(SIGN, 1, h(1)));
        assertSw(0x6A95, c.tx(SIGN, 2, h(1)));
        assertEquals(ST_NOTE, c.slotState(2), "refused: the note is still there");
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(2000, u64(st, 48));
        // a spent slot's own transaction again costs nothing
        assertSw(0x9000, c.tx(SIGN, 0, h(1)));
    }

    @Test
    void aRealHeaderADayLaterStartsANewDay() {
        CardTestSupport c = card(4, 1000, 2000);
        assertSw(0x9000, c.tx(SET_HEADER, 0, H900000));
        assertSw(0x9000, c.tx(SIGN, 0, h(1)));
        assertSw(0x9000, c.tx(SIGN, 1, h(1)));
        assertSw(0x6A95, c.tx(SIGN, 2, h(1)));
        // 900200 is 121 243 s later: a new day
        ResponseAPDU r = c.tx(SET_HEADER, 0, H900200);
        assertSw(0x9000, r);
        assertEquals(1749309742L, u32(r.getData(), 0));
        assertSw(0x9000, c.tx(SIGN, 2, h(2)));
        // 900201 is the same day: 3000 would pass 2000
        assertSw(0x9000, c.tx(SET_HEADER, 0, H900201));
        assertSw(0x9000, c.tx(SIGN, 3, h(3)));
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(2000, u64(st, 48));
        assertEquals(1749309742L, u32(st, 36), "the window began at 900200");
    }

    @Test
    void headersWithoutTheWorkAreRefusedAndAnOlderOneMovesNothing() {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6A93, c.tx(SET_HEADER, 0, GENESIS), "under the floor");
        byte[] forged = H900200.clone();
        forged[79] ^= 1;   // another nonce: the hash no longer meets the target
        assertSw(0x6A93, c.tx(SET_HEADER, 0, forged));
        assertSw(0x9000, c.tx(SET_HEADER, 0, H900200));
        ResponseAPDU r = c.tx(SET_HEADER, 0, H900000);
        assertSw(0x9000, r);
        assertEquals(1749309742L, u32(r.getData(), 0), "the clock never goes back");
        assertSw(0x6700, c.tx(SET_HEADER, 0, new byte[79]));
    }

    // ── attestation ─────────────────────────────────────────────────────

    static final BigInteger P256_P = new BigInteger("FFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF", 16);

    static PublicKey p256(byte[] w) throws Exception {
        java.security.AlgorithmParameters ap = java.security.AlgorithmParameters.getInstance("EC");
        ap.init(new java.security.spec.ECGenParameterSpec("secp256r1"));
        java.security.spec.ECParameterSpec spec = ap.getParameterSpec(java.security.spec.ECParameterSpec.class);
        ECPoint pt = new ECPoint(new BigInteger(1, Arrays.copyOfRange(w, 1, 33)), new BigInteger(1, Arrays.copyOfRange(w, 33, 65)));
        return KeyFactory.getInstance("EC").generatePublic(new ECPublicKeySpec(pt, spec));
    }

    @Test
    void theCardAttestsOverTheChallengeAndItsOwnKey() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] slot0 = c.genKey(0);
        assertSw(0x6985, c.tx(ATTEST, 0, h(7)), "no key, no certificate");
        ResponseAPDU init = c.tx(ATTEST_INIT, 0, null);
        assertSw(0x9000, init);
        byte[] w = init.getData();
        assertEquals(65, w.length);
        assertSw(0x6985, c.tx(ATTEST, 0, h(7)), "no certificate yet");
        byte[] certBytes = fill(130, 0x5C);   // the registry's certificate is 130 bytes
        assertSw(0x9000, c.tx(SET_CERT, 0, certBytes));
        assertArrayEquals(certBytes, c.tx(GET_CERT, 0, null).getData());
        byte[] challenge = new byte[32];
        new java.util.Random(9).nextBytes(challenge);
        ResponseAPDU r = c.tx(ATTEST, 0, challenge);
        assertSw(0x9000, r);
        java.security.Signature v = java.security.Signature.getInstance("SHA256withECDSA");
        v.initVerify(p256(w));
        v.update("fedimintcard/at1".getBytes(java.nio.charset.StandardCharsets.US_ASCII));
        v.update(challenge);
        v.update(slot0);
        assertTrue(v.verify(r.getData()), "ECDSA over label || challenge || slot 0's key");
        // provisioning only: no second certificate, no new key once a PIN is set
        assertSw(0x6985, c.tx(SET_CERT, 0, certBytes));
        assertSw(0x6985, c.tx(ATTEST_INIT, 0, null));
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(130, st[56] & 0xFF);
        assertEquals(1, st[57]);
    }

    @Test
    void attestationKeyIsNotMadeAfterThePin() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        assertSw(0x6985, c.tx(ATTEST_INIT, 0, null));
    }
}
