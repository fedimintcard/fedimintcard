package io.github.bobodread876.fedimintcard.applet;

import com.licel.jcardsim.base.SimulatorRuntime;
import com.licel.jcardsim.smartcardio.CardSimulator;
import com.licel.jcardsim.utils.AIDUtil;
import java.lang.reflect.Field;
import java.math.BigInteger;
import java.util.Arrays;
import javacard.framework.AID;
import javacard.framework.Applet;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;

/** A fresh simulated card with the applet installed and selected, plus helpers. */
final class CardTestSupport {
    static final String AID_HEX = "F0464D4341524401";
    static final int CLA = 0xB0;
    static final int GEN_KEY = 0x10, GET_PUB = 0x11, SIGN = 0x12, STATE_KEY = 0x13;
    static final int VERIFY_PIN = 0x40, SET_PIN = 0x41, CHANGE_PIN = 0x42;
    static final int SET_NOTE = 0x14, GET_SLOT = 0x15, GET_STATE = 0x16, SET_CHANGE = 0x17, SET_LIMIT = 0x18,
        SET_HEADER = 0x19, ATTEST_INIT = 0x30, SET_CERT = 0x31, GET_CERT = 0x32, ATTEST = 0x33,
        SET_REGISTRY_KEY = 0x34, RELEASE = 0x35;
    static final int ST_EMPTY = 0, ST_KEYED = 1, ST_NOTE = 2, ST_CHANGE = 3, ST_SPENT = 4;

    /** A note's data for SET_NOTE: the amount in msat (8, big-endian), then 48 bytes standing for the signature. */
    static byte[] noteData(long msat) {
        byte[] d = new byte[56];
        for (int i = 0; i < 8; i++) d[7 - i] = (byte) (msat >>> (8 * i));
        for (int i = 8; i < 56; i++) d[i] = (byte) (0xA0 + i);
        return d;
    }

    /**
     * SET_NOTE's data for this card's slot as the registry sends it: noteData(msat), then the test registry key's
     * signature over the slot's own x-only key (Registry). A card with no PIN and no registry key gets the test key
     * first, as provisioning sets it.
     */
    byte[] note(int slot, long msat) {
        try {
            if (pinState() == 0 && !(Boolean) field("registryKeySet")) assertSw(0x9000, tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
        return Registry.signed(slotX(slot), noteData(msat));
    }

    /** The slot's x-only key as GET_SLOT answers it (zeros for an empty slot). */
    byte[] slotX(int slot) {
        ResponseAPDU r = tx(GET_SLOT, slot, null);
        assertSw(0x9000, r);
        return Arrays.copyOfRange(r.getData(), 1, 33);
    }

    /** The slot's state byte (test-only reflection). */
    int slotState(int slot) {
        try {
            return ((byte[]) field("state"))[slot];
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    /** A PIN every helper uses unless a test says otherwise: inside the 4..8 byte bounds. */
    static final byte[] DEFAULT_PIN = { '1', '2', '3', '4' };

    static final class ExposedRuntime extends SimulatorRuntime {
        Applet appletAt(AID aid) { return getApplet(aid); }
    }

    final ExposedRuntime runtime = new ExposedRuntime();
    final CardSimulator sim = new CardSimulator(runtime);
    final AID aid = AIDUtil.create(AID_HEX);

    CardTestSupport() {
        sim.installApplet(aid, FedimintCardApplet.class);
        sim.selectApplet(aid);
    }

    /**
     * Power-cycle the card without reinstalling it: persistent state (keys, PIN, retry counter)
     * survives, transient state (the session's verified flag) is cleared. Leaves the applet
     * selected.
     */
    CardTestSupport powerCycle() {
        sim.reset();
        sealer.forget();
        select();
        return this;
    }

    /** SELECT the applet by AID, the way a reader does. */
    ResponseAPDU select() {
        sealer.forget();
        return sim.transmitCommand(new CommandAPDU(0x00, 0xA4, 0x04, 0x00, Bip340.hex(AID_HEX)));
    }

    /** The host's side of the sealed PIN: the PIN commands a test sends in their P1 00 form go sealed (Sealer). */
    final Sealer sealer = new Sealer(sim::transmitCommand);

    ResponseAPDU tx(int cla, int ins, int p1, int p2, byte[] data) {
        return sealer.send(cla, ins, p1, p2, data);
    }

    ResponseAPDU tx(int ins, int p1, byte[] data) { return tx(CLA, ins, p1, 0, data); }

    /**
     * A registry key for tests that need one and never check a registry signature: P-256's generator. SET_PIN takes
     * only a card with a registry key.
     */
    static final byte[] TEST_REGISTRY_KEY = Bip340.hex("046B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C2964FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5");

    /**
     * SET_PIN as provisioning sends it: on a card with no PIN, the test registry key first if none is set, and the
     * set-up's anchor (a header, in SET_PIN's selection), then the PIN.
     */
    ResponseAPDU setPinRaw(byte[] pin) {
        try {
            if (pinState() == 0 && !(Boolean) field("registryKeySet")) assertSw(0x9000, tx(SET_REGISTRY_KEY, 0, TEST_REGISTRY_KEY));
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
        if (pinState() == 0) {
            certify();
            anchor();
        }
        return tx(SET_PIN, 0, pin);
    }

    /** ATTEST_INIT (once) and a stand-in SET_CERT on a card with no certificate: the provisioning a sealed SET_PIN needs. */
    void certify() {
        try {
            if (((byte[]) field("cert"))[0] != 0) return;
            if (!(Boolean) field("attestMade")) assertSw(0x9000, tx(0x30, 0, null), "ATTEST_INIT");
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
        assertSw(0x9000, tx(0x31, 0, TEST_CERT), "SET_CERT");
    }

    /** A stand-in certificate for tests that never check one: GET_NONCE, so a sealed SET_PIN, needs the card to hold one. */
    static final byte[] TEST_CERT = fill(16, 0xCE);


    /** SET_HEADER with the set-up's anchor (Headers.anchor), answered 9000. */
    void anchor() { assertSw(0x9000, tx(SET_HEADER, 0, Headers.anchor()), "the set-up's anchor"); }

    ResponseAPDU verifyPinRaw(byte[] pin) { return tx(VERIFY_PIN, 0, pin); }

    ResponseAPDU changePinRaw(byte[] oldAndNew) { return tx(CHANGE_PIN, 0, oldAndNew); }

    void setPin(byte[] pin) { assertSw(0x9000, setPinRaw(pin)); }

    void verifyPin(byte[] pin) { assertSw(0x9000, verifyPinHeld(pin)); }

    /** VERIFY_PIN as a host sends it: the same command again while the card answers 6A98 (a wait owed), then its answer. */
    ResponseAPDU verifyPinHeld(byte[] pin) {
        ResponseAPDU r;
        int n = 0;
        while ((r = verifyPinRaw(pin)).getSW() == 0x6A98) {
            if (++n > 200) throw new AssertionError("a wait that does not go down");
        }
        return r;
    }

    /**
     * The ordinary provisioning path in one call: SET_PIN (only if no PIN is set yet), VERIFY_PIN,
     * then SIGN. For tests whose subject is not the PIN.
     */
    byte[] signPin(byte[] pin, int slot, byte[] msg) {
        if (pinState() == 0) setPin(pin);
        verifyPin(pin);
        return sign(slot, msg);
    }

    /**
     * The transient memory class of a field, as jCardSim recorded it: {@code CLEAR_ON_DESELECT},
     * {@code CLEAR_ON_RESET}, or 0 for a persistent array. Test-only reflection: the applet has no
     * command that reports it, and the real deselect a {@code CLEAR_ON_DESELECT} answer depends on
     * cannot be driven through jCardSim's card manager.
     */
    int transientClass(String name) throws ReflectiveOperationException {
        Field tmField = SimulatorRuntime.class.getDeclaredField("transientMemory");
        tmField.setAccessible(true);
        Object tm = tmField.get(runtime);
        java.lang.reflect.Method isTransient = tm.getClass().getMethod("isTransient", Object.class);
        return (Byte) isTransient.invoke(tm, (Object) field(name));
    }

    /** The persistent PIN state byte: 0 not set, 1 set. */
    int pinState() {
        try {
            return (Byte) field("pinState");
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    /** The OwnerPIN's retry counter (test-only reflection; no command reports it except 63CX). */
    int triesRemaining() {
        try {
            return ((javacard.framework.OwnerPIN) field("pin")).getTriesRemaining();
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    byte[] genKey(int slot) {
        ResponseAPDU r = tx(GEN_KEY, slot, null);
        assertSw(0x9000, r);
        return r.getData();
    }

    byte[] getPub(int slot) {
        ResponseAPDU r = tx(GET_PUB, slot, null);
        assertSw(0x9000, r);
        return r.getData();
    }

    /**
     * SIGN as the crypto tests use it: a KEYED slot gets a note first (SET_NOTE, 1000 msat), and a
     * slot already spent on another message gets a fresh note on the same key by reflection. That
     * second step is test-only: 3.0 signs one transaction per note (OfflineTest covers the rule),
     * and these tests are about the signatures, not the burn.
     */
    byte[] sign(int slot, byte[] msg) {
        if (slotState(slot) == ST_KEYED) assertSw(0x9000, tx(SET_NOTE, slot, note(slot, 1000)));
        if (slotState(slot) == ST_SPENT) {
            try {
                byte[] spent = (byte[]) field("spentHash");
                if (!Arrays.equals(Arrays.copyOfRange(spent, slot * 32, slot * 32 + 32), msg)) {
                    ((byte[]) field("state"))[slot] = (byte) ST_NOTE;
                }
            } catch (ReflectiveOperationException e) {
                throw new IllegalStateException(e);
            }
        }
        ResponseAPDU r = tx(SIGN, slot, msg);
        assertSw(0x9000, r);
        return r.getData();
    }

    static void assertSw(int sw, ResponseAPDU r) {
        assertSw(sw, r, "");
    }

    static void assertSw(int sw, ResponseAPDU r, String message) {
        // Upper case both sides: Integer.toHexString gives "63c2", the SWs above are written 0x63C2.
        org.junit.jupiter.api.Assertions.assertEquals(
            Integer.toHexString(sw).toUpperCase(java.util.Locale.ROOT),
            Integer.toHexString(r.getSW()).toUpperCase(java.util.Locale.ROOT),
            message);
    }

    private Object applet() { return runtime.appletAt(aid); }

    Object field(String name) throws ReflectiveOperationException {
        Field f = FedimintCardApplet.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(applet());
    }

    /**
     * Every persistent array and every byte, short and boolean field of the applet, copied, for a test that asserts a
     * command wrote nothing (test-only reflection). Key objects are left out: no header command touches one.
     */
    java.util.Map<String, Object> persistent() throws ReflectiveOperationException {
        java.util.Map<String, Object> m = new java.util.TreeMap<>();
        for (Field f : FedimintCardApplet.class.getDeclaredFields()) {
            if (java.lang.reflect.Modifier.isStatic(f.getModifiers())) continue;
            f.setAccessible(true);
            Object v = f.get(applet());
            if (v instanceof byte[]) {
                if (transientClass(f.getName()) == 0) m.put(f.getName(), Arrays.toString((byte[]) v));
            } else if (v instanceof Byte || v instanceof Short || v instanceof Boolean) {
                m.put(f.getName(), v);
            }
        }
        return m;
    }

    /** Puts `v` in the applet's field `name` (test-only reflection). */
    void setField(String name, Object v) throws ReflectiveOperationException {
        Field f = FedimintCardApplet.class.getDeclaredField(name);
        f.setAccessible(true);
        f.set(applet(), v);
    }

    /** The private scalar in a slot (test-only reflection; the applet has no such command). */
    BigInteger scalar(int slot) throws ReflectiveOperationException {
        ECPrivateKey k = ((ECPrivateKey[]) field("privs"))[slot];
        byte[] buf = new byte[64];
        short n = k.getS(buf, (short) 0);
        return new BigInteger(1, Arrays.copyOf(buf, n));
    }

    /** Install a chosen key into a slot, bypassing GEN_KEY. */
    void installKey(int slot, BigInteger d) throws ReflectiveOperationException {
        byte[] s = Bip340.bytes32(d);
        byte[] w = Bip340.uncompressed(Bip340.pubkey(d));
        // Slots get their key objects at first GEN_KEY (keysFor); build them the same way here.
        java.lang.reflect.Method keysFor = applet().getClass().getDeclaredMethod("keysFor", short.class);
        keysFor.setAccessible(true);
        keysFor.invoke(applet(), (short) slot);
        // The curve first, as GEN_KEY sets it: the signer's check works on the slot key's curve.
        Secp256k1.curve(((ECPrivateKey[]) field("privs"))[slot]);
        Secp256k1.curve(((ECPublicKey[]) field("pubs"))[slot]);
        ((ECPrivateKey[]) field("privs"))[slot].setS(s, (short) 0, (short) 32);
        ((ECPublicKey[]) field("pubs"))[slot].setW(w, (short) 0, (short) 65);
        ((byte[]) field("state"))[slot] = (byte) ST_KEYED;
    }

    /** installKey plus the selected-PIN session, so a test can go straight to SIGN. */
    void installKeyAndPin(int slot, BigInteger d, byte[] pin) throws ReflectiveOperationException {
        installKey(slot, d);
        if (pinState() == 0) setPin(pin);
        verifyPin(pin);
    }

    static byte[] fill(int len, int v) {
        byte[] b = new byte[len];
        Arrays.fill(b, (byte) v);
        return b;
    }
}
