package io.github.bobodread876.fedimintcard.applet.proof;

import com.licel.jcardsim.base.SimulatorRuntime;
import com.licel.jcardsim.smartcardio.CardSimulator;
import com.licel.jcardsim.utils.AIDUtil;
import io.github.bobodread876.fedimintcard.applet.Bip340;
import io.github.bobodread876.fedimintcard.applet.FedimintCardApplet;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import javacard.framework.AID;
import javacard.framework.Applet;
import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;

/**
 * The proof tests' card: the same set-up as the applet tests' CardTestSupport (which is package-private there), plus
 * the header mining of Cashu card 3.1's tests. Reflection is used only where a test says so.
 */
final class SimCard {
    static final String AID_HEX = "F0464D4341524401";
    static final int CLA = 0xB0;
    static final int GEN_KEY = 0x10, SIGN = 0x12, SET_NOTE = 0x14, GET_SLOT = 0x15, GET_STATE = 0x16,
        SET_LIMIT = 0x18, SET_HEADER = 0x19, VERIFY_PIN = 0x40, SET_PIN = 0x41, CHANGE_PIN = 0x42;
    static final int ST_EMPTY = 0, ST_KEYED = 1, ST_NOTE = 2, ST_SPENT = 4;
    static final byte[] PIN = { '1', '2', '3', '4' };
    static final byte[] WRONG = { '9', '9', '9', '9' };

    static final class ExposedRuntime extends SimulatorRuntime {
        Applet appletAt(AID aid) { return getApplet(aid); }
    }

    final ExposedRuntime runtime = new ExposedRuntime();
    final CardSimulator sim = new CardSimulator(runtime);
    final AID aid = AIDUtil.create(AID_HEX);

    SimCard() {
        sim.installApplet(aid, FedimintCardApplet.class);
        sim.selectApplet(aid);
    }

    ResponseAPDU select() {
        return sim.transmitCommand(new CommandAPDU(0x00, 0xA4, 0x04, 0x00, Bip340.hex(AID_HEX)));
    }

    /** Power off and on: persistent state stays, transient state goes. Leaves the applet selected. */
    SimCard powerCycle() {
        sim.reset();
        select();
        return this;
    }

    ResponseAPDU tx(int cla, int ins, int p1, int p2, byte[] data) {
        return sim.transmitCommand(data == null || data.length == 0
            ? new CommandAPDU(cla, ins, p1, p2)
            : new CommandAPDU(cla, ins, p1, p2, data));
    }

    ResponseAPDU tx(int ins, int p1, byte[] data) { return tx(CLA, ins, p1, 0, data); }

    int sw(int ins, int p1, byte[] data) { return tx(ins, p1, data).getSW(); }

    /** P-256's generator: a registry key for tests that never check a registry signature. */
    static final byte[] TEST_REGISTRY_KEY = Bip340.hex("046B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C2964FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5");

    /** SET_PIN as provisioning sends it: the registry key first (SET_PIN takes only a card with one), then the PIN. */
    int setPin() {
        int r = sw(0x34, 0, TEST_REGISTRY_KEY);
        if (r != 0x9000) return r;
        return sw(SET_PIN, 0, PIN);
    }

    Applet applet() { return runtime.appletAt(aid); }

    Object field(String name) throws ReflectiveOperationException {
        Field f = FedimintCardApplet.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(applet());
    }

    int slotState(int slot) throws ReflectiveOperationException { return ((byte[]) field("state"))[slot]; }

    int pinState() throws ReflectiveOperationException { return (Byte) field("pinState"); }

    /** GET_STATE: states (32) || now (4) || window (4) || limit (8) || spent today (8) || ... */
    byte[] state() {
        ResponseAPDU r = tx(GET_STATE, 0, null);
        if (r.getSW() != 0x9000) throw new IllegalStateException(Integer.toHexString(r.getSW()));
        return r.getData();
    }

    long now() { return u(state(), 32, 4); }
    long window() { return u(state(), 36, 4); }
    long spentToday() { return u(state(), 48, 8); }

    /** The transient class jCardSim recorded for a field: CLEAR_ON_DESELECT (2), CLEAR_ON_RESET (1), or 0. */
    int transientClass(String name) throws ReflectiveOperationException {
        Field tmField = SimulatorRuntime.class.getDeclaredField("transientMemory");
        tmField.setAccessible(true);
        Object tm = tmField.get(runtime);
        Method isTransient = tm.getClass().getMethod("isTransient", Object.class);
        return (Byte) isTransient.invoke(tm, field(name));
    }

    static long u(byte[] b, int off, int len) {
        long v = 0;
        for (int i = 0; i < len; i++) v = (v << 8) | (b[off + i] & 0xFF);
        return v;
    }

    /** SET_NOTE's data: the amount in msat (8, big-endian), then 48 bytes standing for the federation's signature. */
    static byte[] noteData(long msat) {
        byte[] d = new byte[56];
        for (int i = 0; i < 8; i++) d[7 - i] = (byte) (msat >>> (8 * i));
        for (int i = 8; i < 56; i++) d[i] = (byte) (0xA0 + i);
        return d;
    }

    static byte[] be64(long v) {
        byte[] d = new byte[8];
        for (int i = 0; i < 8; i++) d[7 - i] = (byte) (v >>> (8 * i));
        return d;
    }

    // ── Headers, as Cashu card 3.1's tests make them (CashuAppletTest.header/mine/useFloor) ──

    /** The tests' floor: a target of about 2^255, half of all hashes. Stands for the applet's built floor 0x17087BC0. */
    static final long TEST_BITS = 0x207FFFFFL;
    /** Twice the tests' floor's work: stands for a real recent block, which has about four times the built floor's work. */
    static final long CLOCK_BITS = 0x203FFFFFL;

    static byte[] le32(long v) {
        return new byte[] { (byte) v, (byte) (v >>> 8), (byte) (v >>> 16), (byte) (v >>> 24) };
    }

    static byte[] sha256(byte[] b) {
        try {
            return MessageDigest.getInstance("SHA-256").digest(b);
        } catch (java.security.NoSuchAlgorithmException e) {
            throw new IllegalStateException(e);
        }
    }

    static BigInteger targetOf(long bits) {
        int exponent = (int) ((bits >>> 24) & 0xFF);
        BigInteger mantissa = BigInteger.valueOf(bits & 0x007FFFFFL);
        return exponent <= 3 ? mantissa.shiftRight(8 * (3 - exponent)) : mantissa.shiftLeft(8 * (exponent - 3));
    }

    /** An 80-byte header of that time and bits whose double SHA-256 is at or under its own target. */
    static byte[] mine(long time, long bits) {
        BigInteger target = targetOf(bits);
        byte[] prev = sha256(("previous block " + time).getBytes(StandardCharsets.US_ASCII));
        byte[] merkle = sha256(("transactions " + time).getBytes(StandardCharsets.US_ASCII));
        for (long nonce = 0; nonce < (1L << 32); nonce++) {
            byte[] h = new byte[80];
            System.arraycopy(le32(0x20000000L), 0, h, 0, 4);
            System.arraycopy(prev, 0, h, 4, 32);
            System.arraycopy(merkle, 0, h, 36, 32);
            System.arraycopy(le32(time), 0, h, 68, 4);
            System.arraycopy(le32(bits), 0, h, 72, 4);
            System.arraycopy(le32(nonce), 0, h, 76, 4);
            byte[] d = sha256(sha256(h));
            byte[] r = new byte[32];
            for (int i = 0; i < 32; i++) r[i] = d[31 - i];
            if (new BigInteger(1, r).compareTo(target) <= 0) return h;
        }
        throw new IllegalStateException("no nonce");
    }

    /** The applet's FLOOR_BITS (a private static final array), written in place. */
    static void useFloor(byte[] bits) {
        try {
            Field f = FedimintCardApplet.class.getDeclaredField("FLOOR_BITS");
            f.setAccessible(true);
            System.arraycopy(bits, 0, (byte[]) f.get(null), 0, 4);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    static byte[] floorNow() {
        try {
            Field f = FedimintCardApplet.class.getDeclaredField("FLOOR_BITS");
            f.setAccessible(true);
            return ((byte[]) f.get(null)).clone();
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }
}
