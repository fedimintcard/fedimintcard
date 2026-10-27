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

    ResponseAPDU tx(int cla, int ins, int p1, int p2, byte[] data) {
        return sim.transmitCommand(data == null || data.length == 0
            ? new CommandAPDU(cla, ins, p1, p2)
            : new CommandAPDU(cla, ins, p1, p2, data));
    }

    ResponseAPDU tx(int ins, int p1, byte[] data) { return tx(CLA, ins, p1, 0, data); }

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
        org.junit.jupiter.api.Assertions.assertEquals(
            Integer.toHexString(sw), Integer.toHexString(r.getSW()));
    }

    private Object applet() { return runtime.appletAt(aid); }

    Object field(String name) throws Exception {
        Field f = FedimintCardApplet.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(applet());
    }

    /** The private scalar in a slot (test-only reflection; the applet has no such command). */
    BigInteger scalar(int slot) throws Exception {
        ECPrivateKey k = ((ECPrivateKey[]) field("privs"))[slot];
        byte[] buf = new byte[64];
        short n = k.getS(buf, (short) 0);
        return new BigInteger(1, Arrays.copyOf(buf, n));
    }

    /** Install a chosen key into a slot, bypassing GEN_KEY. */
    void installKey(int slot, BigInteger d) throws Exception {
        byte[] s = Bip340.bytes32(d);
        byte[] w = Bip340.uncompressed(Bip340.pubkey(d));
        ((ECPrivateKey[]) field("privs"))[slot].setS(s, (short) 0, (short) 32);
        ((ECPublicKey[]) field("pubs"))[slot].setW(w, (short) 0, (short) 65);
        ((boolean[]) field("used"))[slot] = true;
    }

    static byte[] fill(int len, int v) {
        byte[] b = new byte[len];
        Arrays.fill(b, (byte) v);
        return b;
    }
}
