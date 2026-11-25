package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.AlgorithmParameters;
import java.security.KeyFactory;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECParameterSpec;
import java.security.spec.ECPoint;
import java.security.spec.ECPublicKeySpec;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyPair;
import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;

import org.junit.jupiter.api.Test;

/**
 * The sealed PIN on the card (docs/OFFLINE-PROTOCOL.md § 9): the shared vectors (spec/vectors/sealed-pin.json, made by
 * host/card/tests/sealed_pin_vectors.rs) put to the applet, Cashu's sealed-PIN tests adapted to our words, and the
 * applet's own sealed-PIN rules. Raw envelopes go to the card unchanged (sim.transmitCommand); Sealer builds
 * them from the spec's formulas, never from the applet's code.
 */
class SealedPinTest {
    static final Path VECTORS = Path.of("..", "spec", "vectors", "sealed-pin.json");
    static final int GET_NONCE = 0x44, SIGN = 0x12, ATTEST_INIT = 0x30, SET_CERT = 0x31;

    static byte[] pin(String s) { return s.getBytes(StandardCharsets.US_ASCII); }

    static ResponseAPDU raw(CardTestSupport c, CommandAPDU a) { return c.sim.transmitCommand(a); }

    static CommandAPDU sealed(CardTestSupport c, int ins, byte[] clear) { return c.sealer.sealedCommand(ins, clear); }

    static CommandAPDU with(int ins, byte[] env) { return new CommandAPDU(0xB0, ins, 0x01, 0x00, env); }

    /** A card with PIN 1234 set, in a new selection that does not have it. */
    static CardTestSupport provisioned() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        c.select();
        return c;
    }

    // ---- the shared vectors ---------------------------------------------------------------------------------------

    static List<Map<String, String>> objects(String json, String having) {
        List<Map<String, String>> out = new ArrayList<>();
        Matcher obj = Pattern.compile("\\{[^{}]*\"" + having + "\"[^{}]*\\}").matcher(json);
        while (obj.find()) {
            Map<String, String> m = new HashMap<>();
            Matcher kv = Pattern.compile("\"(\\w+)\"\\s*:\\s*(\"([^\"]*)\"|true|false|\\d+)").matcher(obj.group());
            while (kv.find()) m.put(kv.group(1), kv.group(3) != null ? kv.group(3) : kv.group(2));
            out.add(m);
        }
        return out;
    }

    /** The vectors' PIN key (its scalar and point) and nonce put into the card, as a GET_NONCE would leave it live. */
    static void holding(CardTestSupport c, Map<String, String> v) throws Exception {
        BigInteger d = new BigInteger(v.get("pin_scalar") == null ? PIN_SCALAR : v.get("pin_scalar"), 16);
        KeyPair k = (KeyPair) c.field("pinKey");
        ((ECPrivateKey) k.getPrivate()).setS(Bip340.bytes32(d), (short) 0, (short) 32);
        ((ECPublicKey) k.getPublic()).setW(Bip340.uncompressed(Bip340.pubkey(d)), (short) 0, (short) 65);
        System.arraycopy(Bip340.hex(v.get("card_nonce")), 0, (byte[]) c.field("nonce"), 0, 16);
        ((byte[]) c.field("held"))[FedimintCardApplet.H_NONCE] = 1;
    }

    static String PIN_SCALAR;

    /** The PIN in a PIN block, or 1234 where the block's length is not one the card takes. */
    static byte[] pinOf(String blockHex, int at) {
        byte[] b = Bip340.hex(blockHex);
        int n = b[at] & 0xFF;
        return n >= 4 && n <= 8 ? Arrays.copyOfRange(b, at + 1, at + 1 + n) : DEFAULT_PIN;
    }

    @Test
    void theSharedVectorsOpenOnTheCardAsTheyDoAtTheReference() throws Exception {
        String json = Files.readString(VECTORS);
        Matcher sc = Pattern.compile("\"card\"\\s*:\\s*\\{[^{}]*\"pin_scalar\"\\s*:\\s*\"([0-9a-f]{64})\"").matcher(json);
        assertTrue(sc.find(), "the vectors' card");
        PIN_SCALAR = sc.group(1);
        List<Map<String, String>> sealed = objects(json, "sealed_for_ins");
        assertEquals(22, sealed.size(), "the vectors' sealed envelopes");
        for (Map<String, String> v : sealed) {
            String name = v.get("name");
            int ins = Integer.parseInt(v.get("ins"), 16);
            int sw = Integer.parseInt(v.get("sw"), 16);
            CardTestSupport c = new CardTestSupport();
            byte[] after = null;
            if (ins == 0x41) {
                c.certify();
                assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
                c.anchor();
                after = pinOf(v.get("clear"), 0);
            } else if (ins == 0x40) {
                c.setPin(pinOf(v.get("clear"), 0));
                c.select();
                after = pinOf(v.get("clear"), 0);
            } else {
                c.setPin(DEFAULT_PIN);
                c.select();
                c.verifyPin(DEFAULT_PIN);
                after = pinOf(v.get("clear"), 9);
            }
            holding(c, v);
            assertSw(sw, raw(c, new CommandAPDU(Bip340.hex(v.get("apdu")))), name);
            assertEquals(0, ((byte[]) c.field("held"))[FedimintCardApplet.H_NONCE], name + ": the nonce is spent");
            if (sw == 0x9000) {
                // the PIN the envelope carried is the card's now (VERIFY's: was already), and only it opens
                c.select();
                assertSw(0x9000, c.verifyPinHeld(after), name + ": its PIN opens");
            }
        }
    }

    // ---- GET_NONCE and the binding ----------------------------------------------------------------------------------

    static PublicKey p256(byte[] w) throws Exception {
        AlgorithmParameters ap = AlgorithmParameters.getInstance("EC");
        ap.init(new ECGenParameterSpec("secp256r1"));
        ECParameterSpec spec = ap.getParameterSpec(ECParameterSpec.class);
        ECPoint pt = new ECPoint(new BigInteger(1, Arrays.copyOfRange(w, 1, 33)), new BigInteger(1, Arrays.copyOfRange(w, 33, 65)));
        return KeyFactory.getInstance("EC").generatePublic(new ECPublicKeySpec(pt, spec));
    }

    static boolean binds(byte[] attestW, byte[] key33, byte[] der) throws Exception {
        Signature v = Signature.getInstance("SHA256withECDSA");
        v.initVerify(p256(attestW));
        v.update(Sealer.cat(Sealer.PIN_LABEL, key33));
        return v.verify(der);
    }

    @Test
    void getNonceNeedsTheCertificateAndNoPinAndBindsThePinKeyUnderTheAttestationKey() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, GET_NONCE, 0x00, 0x00, 256)), "P1 00");
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, GET_NONCE, 0x01, 0x01, 256)), "P2");
        assertSw(0x6985, raw(c, new CommandAPDU(0xB0, GET_NONCE, 0x01, 0x00, 256)), "no certificate");
        ResponseAPDU a = raw(c, new CommandAPDU(0xB0, ATTEST_INIT, 0, 0, 256));
        assertSw(0x9000, a);
        byte[] attestW = a.getData();
        assertSw(0x6985, raw(c, new CommandAPDU(0xB0, GET_NONCE, 0x01, 0x00, 256)), "an attestation key, still no certificate");
        assertSw(0x9000, c.tx(SET_CERT, 0, TEST_CERT));
        assertEquals(0, ((byte[]) c.field("pinKeySig"))[0], "no binding before the first asking");
        Sealer.Nonce first = c.sealer.getNonce();
        assertNotNull(first, "no PIN is needed");
        assertTrue(first.der.length >= 8 && first.der.length <= 72, "a DER of 8..72");
        assertTrue(first.key[0] == 2 || first.key[0] == 3, "a compressed key");
        assertTrue(binds(attestW, first.key, first.der), "the binding verifies under the attestation key");
        byte[] stored = ((byte[]) c.field("pinKeySig")).clone();
        assertEquals(first.der.length, stored[0], "its length kept with it");
        Sealer.Nonce second = c.sealer.getNonce();
        assertArrayEquals(first.key, second.key, "the same key every time");
        assertArrayEquals(first.der, second.der, "and the same binding: made once and kept");
        assertFalse(Arrays.equals(first.nonce, second.nonce), "only the nonce changes");
        assertArrayEquals(stored, (byte[]) c.field("pinKeySig"), "a later asking writes no binding");
        c.setPin(DEFAULT_PIN);
        assertArrayEquals(first.der, c.sealer.getNonce().der, "the binding is fixed for the card's life after SET_CERT");
        assertSw(0x6985, c.tx(ATTEST_INIT, 0, null), "and the keys with it: ATTEST_INIT is refused");
    }

    @Test
    void thePinKeyIsAKeyPairOfItsOwnMadeAtAttestInit() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertNull(c.field("pinKey"), "nothing built at install");
        assertNull(c.field("rsa"), "nor the field keys");
        assertSw(0x9000, c.tx(ATTEST_INIT, 0, null));
        KeyPair k = (KeyPair) c.field("pinKey");
        assertNotNull(c.field("rsaRoot"), "the field keys with it");
        byte[] w1 = new byte[65];
        ((ECPublicKey) k.getPublic()).getW(w1, (short) 0);
        assertSw(0x9000, c.tx(ATTEST_INIT, 0, null));
        assertSame(k, c.field("pinKey"), "built once");
        byte[] w2 = new byte[65];
        ((ECPublicKey) k.getPublic()).getW(w2, (short) 0);
        assertFalse(Arrays.equals(w1, w2), "made again at each ATTEST_INIT before the certificate");
        c.setPin(DEFAULT_PIN);
        assertNotSame(k, c.field("workKeys"), "not the work keys");
        byte[] key = c.sealer.getNonce().key;
        assertFalse(Arrays.equals(Arrays.copyOfRange(key, 1, 33), Arrays.copyOfRange(c.genKey(0), 0, 32)), "not a slot's");
    }

    @Test
    void aTearInsideTheFirstBindingWriteLeavesNoBindingOrTheWholeOne() throws Exception {
        TearHarnessTest.Card c = new TearHarnessTest.Card();
        c.sweepNonce = true;
        TearHook.count = 0;
        assertEquals(0x9000, c.sealer.getNonceRaw().getSW());
        int total = TearHook.count;
        assertTrue(total > 0);
        List<String> wrong = new ArrayList<>();
        for (int k = 1; k <= total; k++) {
            TearHarnessTest.Card t = new TearHarnessTest.Card();
            t.sweepNonce = true;
            TearHook.count = 0;
            TearHook.target = k;
            t.sealer.getNonceRaw();
            String at = TearHook.tornAt;
            TearHook.target = 0;
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            t.powerCycle();
            byte[] w = new byte[65];
            ((ECPublicKey) ((KeyPair) t.field("attest")).getPublic()).getW(w, (short) 0);
            byte[] sig = (byte[]) t.field("pinKeySig");
            ResponseAPDU r = t.sealer.getNonceRaw();
            if (r.getSW() != 0x9000) { wrong.add("write " + k + ": GET_NONCE answers " + Integer.toHexString(r.getSW())); continue; }
            Sealer.Nonce n = new Sealer.Nonce(r.getData());
            if (sig[0] != 0 && sig[0] != n.der.length) wrong.add("write " + k + ": a binding of another length kept");
            if (!binds(w, n.key, n.der)) wrong.add("write " + k + " (" + at + "): a binding that does not verify");
        }
        System.out.println("GET_NONCE binding tear sweep: " + total + " writes, each torn");
        assertEquals(List.of(), wrong);
    }

    // ---- VERIFY_PIN ---------------------------------------------------------------------------------------------------

    @Test
    void aSealedVerifyOfTheRightPinVerifiesOnThePTimesPPath() throws Exception {
        CardTestSupport c = provisioned();
        assertSw(0x9000, raw(c, sealed(c, 0x40, Sealer.block(DEFAULT_PIN))));
        c.genKey(0);
        assertEquals(64, c.sign(0, fill(32, 7)).length, "the session may SIGN");
    }

    @Test
    void theChecksComeInTheSpecsOrderAndRefusalsBeforeTheOpeningLeaveTheNonceLive() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x6984, raw(c, with(0x40, new byte[90])), "VERIFY_PIN: no PIN set");
        c = provisioned();
        CommandAPDU good = sealed(c, 0x40, Sealer.block(DEFAULT_PIN));
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, 0x40, 0x00, 0x00, DEFAULT_PIN)), "P1 00: 6B00, not 6700");
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, 0x40, 0x01, 0x01, good.getData())), "P2");
        assertSw(0x6700, raw(c, with(0x40, Arrays.copyOf(good.getData(), 89))), "an envelope shorter than its parts");
        assertSw(0x6700, raw(c, with(0x40, Arrays.copyOf(good.getData(), 91))), "or longer");
        assertSw(0x9000, raw(c, good), "the refusals spent nothing");
        assertSw(0x6985, raw(c, good), "a sealed command cannot be sent twice");
        c.select();
        CommandAPDU next = sealed(c, 0x40, Sealer.block(DEFAULT_PIN));
        c.select();
        assertSw(0x6985, raw(c, next), "a SELECT ends the nonce");
    }

    @Test
    void aSealedWrongPinCostsATryAndOwesItsWait() throws Exception {
        CardTestSupport c = provisioned();
        assertSw(0x63C1, raw(c, sealed(c, 0x40, Sealer.block(pin("9999")))));
        CommandAPDU right = sealed(c, 0x40, Sealer.block(DEFAULT_PIN));
        assertSw(0x6A98, raw(c, right), "the wait first");
        assertSw(0x9000, raw(c, right), "the same envelope through the wait: the nonce stayed live");
    }

    static byte[] spoiled(byte[] env, int at) {
        byte[] e = env.clone();
        e[at] ^= 1;
        return e;
    }

    @Test
    void anEnvelopeThatDoesNotOpenIsAWrongTry() throws Exception {
        CardTestSupport c = provisioned();
        byte[] env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x63C1, raw(c, with(0x40, spoiled(env, 89))), "the tag");
        c.verifyPin(DEFAULT_PIN);
        env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x63C1, raw(c, with(0x40, spoiled(env, 70))), "the body");
        c.verifyPin(DEFAULT_PIN);
        env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x63C1, raw(c, with(0x40, spoiled(env, 64))), "E off the curve never opens");
        c.verifyPin(DEFAULT_PIN);
        env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        byte[] high = env.clone();
        Arrays.fill(high, 1, 33, (byte) 0xFF);
        assertSw(0x63C1, raw(c, with(0x40, high)), "an X at or over p");
        c.verifyPin(DEFAULT_PIN);
        Sealer.Nonce n = c.sealer.getNonce();
        assertSw(0x63C1, raw(c, with(0x40, Sealer.seal(n.key, n.nonce, 0x41, Sealer.block(DEFAULT_PIN)))), "sealed for SET_PIN");
        c.verifyPin(DEFAULT_PIN);
        n = c.sealer.getNonce();
        assertSw(0x63C1, raw(c, with(0x40, Sealer.seal(n.key, new byte[16], 0x40, Sealer.block(DEFAULT_PIN)))), "a stale nonce");
        assertSw(0x6A98, c.verifyPinRaw(DEFAULT_PIN), "each was a try that owed its wait");
    }

    @Test
    void aBadBlockLengthIs6700AndNoTry() throws Exception {
        CardTestSupport c = provisioned();
        byte[] block = Sealer.block(pin("123"));
        assertSw(0x6700, raw(c, sealed(c, 0x40, block)));
        assertEquals(0, ((byte[]) c.field("pinMarks"))[0], "no try");
        assertEquals(0, ((byte[]) c.field("held"))[FedimintCardApplet.H_NONCE], "the nonce spent");
        block[0] = 9;
        assertSw(0x6700, raw(c, sealed(c, 0x40, block)));
        assertSw(0x9000, c.verifyPinRaw(DEFAULT_PIN), "and no wait");
    }

    @Test
    void aFailedOpeningInTheSetPinSelectionEndsItsSession() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        c.genKey(0);
        byte[] env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x63C1, raw(c, with(0x40, spoiled(env, 89))));
        assertSw(0x6A94, c.tx(SIGN, 0, fill(32, 7)), "SIGN is closed");
        c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        c.genKey(0);
        env = sealed(c, 0x42, Sealer.cat(Sealer.block(DEFAULT_PIN), Sealer.block(pin("5555")))).getData();
        assertSw(0x63C1, raw(c, with(0x42, spoiled(env, 98))), "CHANGE_PIN's failed opening");
        assertSw(0x6A94, c.tx(SIGN, 0, fill(32, 7)), "SIGN is closed");
        c.select();
        assertSw(0x6A98, c.verifyPinRaw(DEFAULT_PIN), "it was a try");
    }

    @Test
    void aGetNonceInTheWaitRestartsTheStretchAndTheOldEnvelopeIsThenAWrongTry() throws Exception {
        CardTestSupport c = provisioned();
        assertSw(0x63C1, c.verifyPinRaw(pin("9999")));
        CommandAPDU right = sealed(c, 0x40, Sealer.block(DEFAULT_PIN));
        assertSw(0x6A98, raw(c, right), "the wait of one asking");
        c.sealer.getNonce();
        assertSw(0x6A98, raw(c, right), "the stretch begins again");
        assertSw(0x63C2, raw(c, right), "then the old envelope fails its tag under the new nonce: a try");
        right = sealed(c, 0x40, Sealer.block(DEFAULT_PIN));
        assertSw(0x6A98, raw(c, right));
        assertSw(0x6A98, raw(c, right), "the next wait doubled");
        assertSw(0x9000, raw(c, right));
    }

    @Test
    void nothingOfASealedCommandOutlivesIt() throws Exception {
        CardTestSupport c = provisioned();
        assertSw(0x9000, raw(c, sealed(c, 0x40, Sealer.block(DEFAULT_PIN))));
        assertArrayEquals(new byte[144], (byte[]) c.field("x"), "the seal's room and reduceModP's");
        assertArrayEquals(new byte[65], (byte[]) c.field("tmp"), "x^3 + 7");
        byte[] env = sealed(c, 0x40, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x63C1, raw(c, with(0x40, spoiled(env, 89))));
        assertArrayEquals(new byte[144], (byte[]) c.field("x"), "after a failed opening too");
        assertArrayEquals(new byte[65], (byte[]) c.field("tmp"));
    }

    // ---- SET_PIN and CHANGE_PIN --------------------------------------------------------------------------------------

    @Test
    void aSealedSetPinIsRefusedInTheSpecsOrderAndBeforeTheEnvelope() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.certify();
        CommandAPDU s = sealed(c, 0x41, Sealer.block(DEFAULT_PIN));
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, 0x41, 0x00, 0x00, DEFAULT_PIN)), "P1 00");
        assertSw(0x6700, raw(c, with(0x41, Arrays.copyOf(s.getData(), 89))), "Lc");
        assertSw(0x6A96, raw(c, s), "not anchored");
        c.anchor();
        assertSw(0x6985, raw(c, s), "no registry key");
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        assertSw(0x9000, raw(c, s), "each refusal left the nonce live");
        assertSw(0x6985, raw(c, s), "a PIN already set");
        c = new CardTestSupport();
        c.certify();
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        c.anchor();
        assertSw(0x6985, raw(c, with(0x41, s.getData())), "no live nonce in this selection");
        c.sealer.getNonce();
        Map<String, Object> before = c.persistent();
        byte[] env = sealed(c, 0x41, Sealer.block(DEFAULT_PIN)).getData();
        assertSw(0x6700, raw(c, with(0x41, spoiled(env, 89))), "an envelope that does not open");
        assertEquals(before, c.persistent(), "writes nothing");
        assertSw(0x6700, raw(c, sealed(c, 0x41, Sealer.block(pin("123")))), "a bad block length");
        assertEquals(before, c.persistent(), "writes nothing");
        assertSw(0x9000, raw(c, sealed(c, 0x41, Sealer.block(pin("87654321")))));
        c.select();
        assertSw(0x9000, c.verifyPinHeld(pin("87654321")));
    }

    @Test
    void aSealedChangePinTakesTwoBlocksAndRefusesInOrder() throws Exception {
        CardTestSupport c = provisioned();
        byte[] two = Sealer.cat(Sealer.block(DEFAULT_PIN), Sealer.block(pin("246810")));
        assertSw(0x6A94, raw(c, sealed(c, 0x42, two)), "no PIN session");
        c.verifyPin(DEFAULT_PIN);
        assertSw(0x6B00, raw(c, new CommandAPDU(0xB0, 0x42, 0x04, 0x00, pin("12345678"))), "P1 00");
        CommandAPDU s = sealed(c, 0x42, two);
        assertSw(0x6700, raw(c, with(0x42, Arrays.copyOf(s.getData(), 90))), "Lc");
        byte[] bad = Sealer.cat(Sealer.block(DEFAULT_PIN), Sealer.block(pin("123")));
        assertSw(0x6700, raw(c, sealed(c, 0x42, bad)), "a new block of a bad length, no try");
        assertEquals(0, ((byte[]) c.field("pinMarks"))[0]);
        assertSw(0x6985, raw(c, s), "the nonce that sealed s was replaced");
        assertSw(0x9000, raw(c, sealed(c, 0x42, two)));
        assertSw(0x6A94, c.tx(SIGN, 0, fill(32, 7)), "the session ends");
        assertSw(0x9000, c.verifyPinHeld(pin("246810")));
    }

    // ---- what the source says, and the room ---------------------------------------------------------------------------

    @Test
    void theSealedPathIsInTheSourceWhatItsDescriptionSays() throws Exception {
        String src = Files.readString(Path.of("src", "main", "java", "io", "github", "bobodread876", "fedimintcard",
            "applet", "FedimintCardApplet.java"));
        assertTrue(FedimintCardApplet.FIELD >= 5 + FedimintCardApplet.SEAL_LC_CHANGE, "the field room is past the longest envelope");
        assertEquals(FedimintCardApplet.FIELD_END, FedimintCardApplet.FIELD + 128, "and 128 bytes");
        assertTrue(src.contains("if ((short) buf.length < FIELD_END) {"), "a shorter buffer is refused");
        int unseal = src.indexOf("private short unseal(");
        String body = src.substring(unseal, src.indexOf("\n    }\n", unseal));
        assertTrue(body.indexOf("requireFullPoint(buf, e);") < body.indexOf("schnorr.agree("), "no point reaches agree unchecked");
        assertTrue(body.indexOf("requireFullPoint(buf, e);") < body.indexOf("x[S_INS] ="), "the INS written after the curve check");
        assertTrue(body.indexOf("held[H_NONCE] = 0;") < body.indexOf("requireFullPoint"), "the nonce spent before anything is tried");
        assertTrue(body.indexOf("sameBytes(x, S_HASH, buf, tag, TAG_LEN)") < body.indexOf("moveDown"), "the tag before the opening");
        for (String m : new String[] { "verifyPin", "setPin", "changePin", "getNonce" }) {
            int at = src.indexOf("private void " + m + "(APDU apdu, byte[] buf) {");
            String b = src.substring(at, src.indexOf("\n    }\n", at));
            assertTrue(b.contains("buf[ISO7816.OFFSET_P1] != SEALED || buf[ISO7816.OFFSET_P2] != 0"), m + " takes P1 = 01 only");
        }
    }

    @Test
    void theClearOnDeselectRequestStaysUnderItsBudget() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        int sum = 0;
        StringBuilder parts = new StringBuilder();
        for (Object o : new Object[] { appletOf(c), c.field("schnorr") }) {
            for (java.lang.reflect.Field f : o.getClass().getDeclaredFields()) {
                if (java.lang.reflect.Modifier.isStatic(f.getModifiers()) || f.getType() != byte[].class) continue;
                f.setAccessible(true);
                byte[] v = (byte[]) f.get(o);
                if (v != null && transientOf(c, v) == 2) {
                    sum += v.length;
                    parts.append(o.getClass().getSimpleName()).append('.').append(f.getName()).append(' ').append(v.length).append("; ");
                }
            }
        }
        System.out.println("CLEAR_ON_DESELECT arrays: " + sum + " B (" + parts + "), key objects and ciphers not counted");
        assertTrue(sum < 931, "the arrays alone under 931 B: " + sum);
    }

    static Object appletOf(CardTestSupport c) { return c.runtime.appletAt(c.aid); }

    static int transientOf(CardTestSupport c, Object array) throws Exception {
        java.lang.reflect.Field tmField = com.licel.jcardsim.base.SimulatorRuntime.class.getDeclaredField("transientMemory");
        tmField.setAccessible(true);
        Object tm = tmField.get(c.runtime);
        return (Byte) tm.getClass().getMethod("isTransient", Object.class).invoke(tm, array);
    }
}
