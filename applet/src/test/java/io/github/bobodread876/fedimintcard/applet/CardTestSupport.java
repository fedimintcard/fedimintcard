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
    static final int GEN_KEY = 0x10, GET_PUB = 0x11, SIGN = 0x12;
    static final int VERIFY_PIN = 0x40, SET_PIN = 0x41, CHANGE_PIN = 0x42;

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
        select();
        return this;
    }

    /** SELECT the applet by AID, the way a reader does. */
    ResponseAPDU select() {
        return sim.transmitCommand(new CommandAPDU(0x00, 0xA4, 0x04, 0x00, Bip340.hex(AID_HEX)));
    }

    ResponseAPDU tx(int cla, int ins, int p1, int p2, byte[] data) {
        return sim.transmitCommand(data == null || data.length == 0
            ? new CommandAPDU(cla, ins, p1, p2)
            : new CommandAPDU(cla, ins, p1, p2, data));
    }

    ResponseAPDU tx(int ins, int p1, byte[] data) { return tx(CLA, ins, p1, 0, data); }

    ResponseAPDU setPinRaw(byte[] pin) { return tx(SET_PIN, 0, pin); }

    ResponseAPDU verifyPinRaw(byte[] pin) { return tx(VERIFY_PIN, 0, pin); }

    ResponseAPDU changePinRaw(byte[] oldAndNew) { return tx(CHANGE_PIN, 0, oldAndNew); }

    void setPin(byte[] pin) { assertSw(0x9000, setPinRaw(pin)); }

    void verifyPin(byte[] pin) { assertSw(0x9000, verifyPinRaw(pin)); }

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

    /** The persistent PIN state byte: 0 not set, 1 set, 2 blocked. */
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

    byte[] sign(int slot, byte[] msg) {
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
        ((ECPrivateKey[]) field("privs"))[slot].setS(s, (short) 0, (short) 32);
        ((ECPublicKey[]) field("pubs"))[slot].setW(w, (short) 0, (short) 65);
        ((boolean[]) field("used"))[slot] = true;
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
