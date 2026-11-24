package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.ChipEnginesTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import javacard.security.KeyAgreement;
import javacardx.crypto.Cipher;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * SET_PIN's rules: once, on a card with the registry key and no value; the selection that sends it has the PIN; the
 * signer's engine check runs before anything is written, and its one-time set-up of the chip's product before the commit
 * point.
 */
class SetPinTest {

    static final byte[] PIN = DEFAULT_PIN;

    static SchnorrHW signer(CardTestSupport c) throws Exception { return (SchnorrHW) c.field("schnorr"); }

    static byte[] xOnly(byte[] pub) { return pub.length == 32 ? pub : Arrays.copyOfRange(pub, 1, 33); }

    /** A key agreement in front of the simulator's whose every answer has its last byte changed. */
    static final class Lying extends KeyAgreement {
        final KeyAgreement real;
        int calls;
        /** Run at the first answer, before it is given. */
        Runnable onFirst;
        Lying(byte alg) { real = KeyAgreement.getInstance(alg, false); }
        @Override public void init(javacard.security.PrivateKey k) { real.init(k); }
        @Override public byte getAlgorithm() { return real.getAlgorithm(); }
        @Override public short generateSecret(byte[] pub, short off, short len, byte[] out, short outOff) {
            if (calls++ == 0 && onFirst != null) onFirst.run();
            short n = real.generateSecret(pub, off, len, out, outOff);
            out[outOff + n - 1] ^= 1;
            return n;
        }
    }

    @Test
    void theSelectionThatSetsThePinHasIt() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(PIN);
        // no VERIFY_PIN: SIGN and STATE_KEY pass the PIN gate
        assertSw(0x9000, c.tx(STATE_KEY, 0, null));
        byte[] pub = c.genKey(0);
        assertSw(0x9000, c.tx(SET_NOTE, 0, c.note(0, 1000)));
        byte[] msg = fill(32, 7);
        ResponseAPDU r = c.tx(SIGN, 0, msg);
        assertSw(0x9000, r);
        assertTrue(Bip340.verify(xOnly(pub), msg, r.getData()));
        // and CHANGE_PIN, whose old PIN is still checked
        assertSw(0x63C1, c.changePinRaw(cat(fill(4, '9'), fill(4, '5'))), "a wrong old PIN is a try");
        assertSw(0x6A94, c.tx(STATE_KEY, 0, null), "and a try ends the given PIN too");
        c.verifyPin(PIN);
        assertSw(0x9000, c.tx(STATE_KEY, 0, null));
    }

    @Test
    void aWrongVerifyAfterSetPinEndsIt() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(PIN);
        assertSw(0x63C1, c.verifyPinRaw(fill(4, '9')));
        assertSw(0x6A94, c.tx(STATE_KEY, 0, null));
    }

    @Test
    void theGivenPinEndsWithTheSelectionAndThePower() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(PIN);
        c.select();
        assertSw(0x6A94, c.tx(STATE_KEY, 0, null), "a new SELECT");
        CardTestSupport d = new CardTestSupport();
        d.setPin(PIN);
        d.powerCycle();
        assertSw(0x6A94, d.tx(STATE_KEY, 0, null), "a power cycle");
    }

    @Test
    void theGivenFlagIsReadBesideThePinState() throws Exception {
        CardTestSupport c = new CardTestSupport();
        ((byte[]) c.field("held"))[FedimintCardApplet.H_PIN_GIVEN] = 1;   // REFLECTION: the flag with no PIN set
        assertSw(0x6984, c.tx(STATE_KEY, 0, null), "the flag alone opens nothing on a card with no PIN");
        assertSw(0x6984, c.changePinRaw(cat(PIN, PIN)));
    }

    @Test
    void refusedWhenAPinIsSetASlotHoldsValueOrNoRegistryKeyIsSet() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.anchor();
        assertSw(0x6985, c.tx(SET_PIN, 0, PIN), "no registry key");
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        c.genKey(0);
        assertSw(0x9000, c.tx(SET_NOTE, 0, c.note(0, 1000)));
        assertSw(0x6985, c.tx(SET_PIN, 0, PIN), "a note");
        ((byte[]) c.field("state"))[0] = ST_CHANGE;   // REFLECTION: change owed to the slot
        assertSw(0x6985, c.tx(SET_PIN, 0, PIN), "change owed");
        ((byte[]) c.field("state"))[0] = ST_KEYED;
        assertEquals(0, c.pinState());
        assertNull(c.field("workKeys"), "a refused SET_PIN makes nothing");
        assertFalse(signer(c).chipMul);
        assertSw(0x9000, c.tx(SET_PIN, 0, PIN), "a keyed slot holds no value");
        assertSw(0x6985, c.tx(SET_PIN, 0, PIN), "a PIN is set");
    }

    @Test
    void theEngineCheckRunsBeforeAnyWrite() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        c.anchor();
        SchnorrHW s = signer(c);
        Object ecdh = get(s, SchnorrHW.class, "ecdh");
        Lying lying = new Lying(KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY);
        set(s, SchnorrHW.class, "ecdh", lying);
        assertSw(0x6F00, c.tx(SET_PIN, 0, PIN), "a chip whose key agreement answers otherwise takes no PIN");
        assertTrue(lying.calls > 0, "the check asked the engine");
        assertEquals(0, c.pinState());
        assertNull(c.field("workKeys"));
        for (byte b : (byte[]) c.field("newPin")) assertEquals(0, b);
        set(s, SchnorrHW.class, "ecdh", ecdh);
        assertSw(0x9000, c.tx(SET_PIN, 0, PIN));
    }

    @Test
    void theEngineCheckComesBeforeThePinIsCopiedAndBeforeTheSignersSetUp() throws Exception {
        for (byte alg : new byte[] { KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY, KeyAgreement.ALG_EC_PACE_GM }) {
            String which = alg == KeyAgreement.ALG_EC_PACE_GM ? "gm" : "ecdh";
            CardTestSupport c = new CardTestSupport();
            assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
            c.anchor();
            SchnorrHW s = signer(c);
            Engine eng = new Engine(Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false));
            FakeKey sq = new FakeKey();
            set(s, SchnorrHW.class, "rsaC", eng);
            set(s, SchnorrHW.class, "sqKey", sq);
            byte[] newPin = (byte[]) c.field("newPin");
            Object[] seen = new Object[3];
            Object real = get(s, SchnorrHW.class, which);
            Lying lying = new Lying(alg);
            lying.onFirst = () -> { seen[0] = newPin.clone(); seen[1] = eng.powers; seen[2] = sq.mod; };
            set(s, SchnorrHW.class, which, lying);
            assertSw(0x6F00, c.tx(SET_PIN, 0, PIN), which + ": a chip whose engine answers otherwise takes no PIN");
            assertTrue(lying.calls > 0, which + ": the check asked the engine");
            for (byte b : (byte[]) seen[0]) assertEquals(0, b, which + ": the PIN was not copied in before the check");
            assertEquals(0, seen[1], which + ": the signer's set-up had not run before the check");
            assertNull(seen[2], which + ": nor set the squaring's key");
            assertFalse(s.chipMul, which + ": the chip's product stays off");
            assertNull(sq.mod, which + ": and its key unset");
            assertEquals(0, c.pinState());
            assertNull(c.field("workKeys"));
            set(s, SchnorrHW.class, which, real);
            assertSw(0x9000, c.tx(SET_PIN, 0, PIN));
            assertTrue(s.chipMul, which + ": with the engine right, SET_PIN runs the set-up");
        }
    }

    /** SET_PIN on a card whose signer has `eng` for the chip's RSA; returns the card with the PIN given. */
    static CardTestSupport cardWith(Engine eng) throws Exception {
        CardTestSupport c = new CardTestSupport();
        SchnorrHW s = signer(c);
        set(s, SchnorrHW.class, "rsaC", eng);
        set(s, SchnorrHW.class, "sqKey", new FakeKey());
        assertFalse(s.chipMul, "no chip path before SET_PIN");
        c.setPin(PIN);
        return c;
    }

    @Test
    void setPinTurnsTheChipsProductOnAndSignUsesIt() throws Exception {
        Engine eng = new Engine(Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false));
        CardTestSupport c = cardWith(eng);
        SchnorrHW s = signer(c);
        assertTrue(s.chipMul, "SET_PIN ran the signer's set-up");
        assertEquals(N, ((FakeKey) get(s, SchnorrHW.class, "sqKey")).mod, "the squaring's key has modulus n");
        assertEquals(1, c.tx(GET_STATE, 0, null).getData()[64] & 1, "GET_STATE says so");
        byte[] pub = c.genKey(0);
        assertSw(0x9000, c.tx(SET_NOTE, 0, c.note(0, 1000)));
        int before = eng.powers;
        byte[] msg = fill(32, 9);
        ResponseAPDU r = c.tx(SIGN, 0, msg);
        assertSw(0x9000, r);
        assertTrue(eng.powers - before >= 2, "SIGN squared on the chip: " + (eng.powers - before));
        assertTrue(Bip340.verify(xOnly(pub), msg, r.getData()));
    }

    @Test
    void aLyingProductLeavesTheChipPathOff() throws Exception {
        Engine eng = new Engine(Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false));
        eng.lie = true;
        CardTestSupport c = cardWith(eng);
        assertFalse(signer(c).chipMul);
        assertEquals(0, c.tx(GET_STATE, 0, null).getData()[64] & 1);
        byte[] pub = c.genKey(0);
        assertSw(0x9000, c.tx(SET_NOTE, 0, c.note(0, 1000)));
        int before = eng.powers;
        byte[] msg = fill(32, 9);
        ResponseAPDU r = c.tx(SIGN, 0, msg);
        assertSw(0x9000, r);
        assertEquals(before, eng.powers, "no squaring on the chip");
        assertTrue(Bip340.verify(xOnly(pub), msg, r.getData()));
    }

    static byte[] cat(byte[] a, byte[] b) {
        byte[] r = Arrays.copyOf(a, a.length + b.length);
        System.arraycopy(b, 0, r, a.length, b.length);
        return r;
    }
}
