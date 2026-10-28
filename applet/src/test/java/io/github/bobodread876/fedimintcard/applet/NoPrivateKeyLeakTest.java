package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.ArrayList;
import java.util.List;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/** No response (data or status word) to any command may contain the private scalar. */
class NoPrivateKeyLeakTest {
    static final int WINDOW = 16;

    /** True when any 16-byte window of a secret occurs in the response, or the full 32 bytes do. */
    static boolean leaks(byte[] response, List<byte[]> secrets) {
        for (byte[] sec : secrets) {
            for (int i = 0; i + WINDOW <= sec.length; i++) {
                for (int j = 0; j + WINDOW <= response.length; j++) {
                    boolean eq = true;
                    for (int k = 0; k < WINDOW && eq; k++) eq = sec[i + k] == response[j + k];
                    if (eq) return true;
                }
            }
        }
        return false;
    }

    private static byte[] wire(ResponseAPDU r) { return r.getBytes(); }

    private static void addKeySecrets(List<byte[]> secrets, BigInteger d) {
        secrets.add(Bip340.bytes32(d));
        secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
    }

    /**
     * The slot's key is chosen, not generated, and its bytes are neither the all-zero 32-byte data
     * the sweeps send nor all-ones: a sweep command that hit it would otherwise be flagged as a
     * key leak when it is only the request bytes echoed by the harness.
     */
    static final BigInteger SWEEP_KEY = new BigInteger(
        "B7E151628AED2A6ABF7158809CF4F3C762E7160F38B4DA56A784D9045190CFEF", 16);

    @Test
    void sweepsLeakNothing() throws Exception {
        CardTestSupport c = new CardTestSupport();
        List<byte[]> secrets = new ArrayList<>();
        List<byte[]> genWire = new ArrayList<>();
        for (int s = 0; s < 8; s++) {
            genWire.add(wire(c.tx(CardTestSupport.GEN_KEY, s, null)));
            addKeySecrets(secrets, c.scalar(s));
        }
        for (byte[] w : genWire) {
            assertEquals(34, w.length);
            assertFalse(leaks(w, secrets), "GEN_KEY response leaked the scalar");
        }
        // The sweeps send PIN commands with bounded Lc, so a wrong PIN here would spend a try.
        // They are allowed to: every sweep starts from a reinstalled card, and each card ends
        // with at most three PIN-bearing APDUs (0x40 at two lengths), so nothing is blocked and
        // the scalars are still wiped by the reinstall.
        int checked = 0;
        // Full INS sweep with a small matrix.
        for (int ins = 0; ins < 256; ins++) {
            for (int p1 : new int[] { 0, 8 }) {
              for (int p2 : new int[] { 0, 1 }) {
                for (int len : new int[] { 0, 32 }) {
                    ResponseAPDU r = c.tx(0xB0, ins, p1, p2, new byte[len]);
                    if (ins == 0x10 && p2 == 1 && p1 == 0 && r.getSW() == 0x9000) {
                        addKeySecrets(secrets, c.scalar(0));
                    }
                    assertFalse(leaks(wire(r), secrets), "INS " + ins + " P1 " + p1 + " len " + len);
                    checked++;
                }
              }
            }
        }
        // Full matrix for the six real commands.
        c = new CardTestSupport();
        for (int s = 0; s < 8; s++) c.genKey(s);
        secrets.clear();
        for (int s = 0; s < 8; s++) addKeySecrets(secrets, c.scalar(s));
        for (int ins = 0x40; ins <= 0x42; ins++) {
            for (int p1 : new int[] { 0, 1 }) {
                for (int p2 : new int[] { 0, 1 }) {
                    for (int len : new int[] { 0, 4, 8, 32 }) {
                        ResponseAPDU r = c.tx(0xB0, ins, p1, p2, new byte[len]);
                        assertFalse(leaks(wire(r), secrets), "INS " + ins + " P1 " + p1 + " len " + len);
                    }
                }
            }
        }
        c = new CardTestSupport();
        for (int s = 0; s < 8; s++) c.genKey(s);
        assertFalse(leaks(wire(c.setPinRaw(CardTestSupport.DEFAULT_PIN)), secrets), "SET_PIN");
        assertFalse(leaks(wire(c.verifyPinRaw(CardTestSupport.DEFAULT_PIN)), secrets), "VERIFY_PIN");
        for (int ins = 0x10; ins <= 0x12; ins++) {
            for (int p1 : new int[] { 0, 1, 2, 3, 7, 8, 0xFF }) {
                for (int p2 : new int[] { 0, 1, 2 }) {
                    for (int len : new int[] { 0, 1, 31, 32, 33, 64, 255 }) {
                        byte[] data = new byte[len];
                        for (int i = 0; i < len; i++) data[i] = (byte) (i * 7 + ins);
                        ResponseAPDU r = c.tx(0xB0, ins, p1, p2, data);
                        byte[] w = wire(r);
                        // P2=1 GEN_KEY replaces a scalar: track the new one too (old ones stay in the list).
                        if (ins == 0x10 && p2 == 1 && p1 >= 0 && p1 < 8 && r.getSW() == 0x9000) {
                            addKeySecrets(secrets, c.scalar(p1));
                        }
                        assertFalse(leaks(w, secrets), "INS " + ins + " P1 " + p1 + " P2 " + p2 + " len " + len);
                        if (ins == 0x12 && r.getSW() == 0x9000) {
                            byte[] s = java.util.Arrays.copyOfRange(r.getData(), 32, 64);
                            for (byte[] sec : secrets) assertFalse(java.util.Arrays.equals(s, sec));
                        }
                        checked++;
                    }
                }
            }
        }
        assertTrue(checked > 1200, "sweep size " + checked);
    }

    /**
     * Same sweep on a card whose one key is known and whose PIN is set and verified: SIGN reaches
     * the signing path, so every response is checked against the key, and SIGN is exercised both
     * at the length that signs and at the length it must refuse.
     *
     * <p>SIGN is gated on a VERIFY_PIN in the same session, so the session is re-opened before
     * each INS 0x12 point. A verified session is what makes the answers here meaningful: without
     * it every SIGN would stop at {@code 6A94} and the signing path would not be swept at all.
     * The re-open is not part of what is measured: only the swept command's response is scanned.
     */
    @Test
    void sweepOnKeyedPinCardLeaksNothing() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.installKeyAndPin(0, SWEEP_KEY, CardTestSupport.DEFAULT_PIN);
        List<byte[]> secrets = new ArrayList<>();
        addKeySecrets(secrets, SWEEP_KEY);
        int signed = 0, refused = 0;
        for (int ins = 0; ins < 256; ins++) {
            for (int len : new int[] { 0, 32 }) {
                if (ins == CardTestSupport.SIGN) c.verifyPin(CardTestSupport.DEFAULT_PIN);
                ResponseAPDU r = c.tx(0xB0, ins, 0, 0, new byte[len]);
                if (ins == CardTestSupport.SIGN) {
                    // The gate is open, so the length is the answer: 32 bytes signs, 0 does not.
                    if (len == 32) {
                        assertEquals(0x9000, r.getSW(), "SIGN of 32 bytes answers");
                        assertEquals(64, r.getData().length);
                        signed++;
                    } else {
                        assertEquals(0x6700, r.getSW(), "SIGN of 0 bytes is a wrong length");
                        refused++;
                    }
                }
                assertFalse(leaks(wire(r), secrets), "INS " + ins + " len " + len);
            }
        }
        // Anti-vacuity: both arms ran, so the sweep really did reach SIGN with the gate open.
        assertEquals(1, signed);
        assertEquals(1, refused);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        byte[] sig = c.tx(CardTestSupport.SIGN, 0, new byte[32]).getData();
        assertFalse(leaks(sig, secrets));
    }

    /** The only INS values the applet answers (with CLA B0) are 0x10, 0x11, 0x12 and 0x40-0x42. */
    @Test
    void onlyTheSixCommandsExist() {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.setPin(CardTestSupport.DEFAULT_PIN);
        List<Integer> known = new ArrayList<>();
        for (int ins = 0; ins < 256; ins++) {
            if (c.tx(0xB0, ins, 0, 0, null).getSW() != 0x6D00) known.add(ins);
        }
        assertEquals(List.of(0x10, 0x11, 0x12, 0x40, 0x41, 0x42), known);
    }

    /** Anti-vacuity: the scan flags a planted scalar, a planted fragment, and its negation. */
    @Test
    void scanDetectsPlantedSecret() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        byte[] d = Bip340.bytes32(c.scalar(0));
        byte[] nd = Bip340.bytes32(Bip340.N.subtract(c.scalar(0)));
        List<byte[]> secrets = List.of(d, nd);

        byte[] planted = new byte[80];
        System.arraycopy(d, 0, planted, 10, 32);
        assertTrue(leaks(planted, secrets));

        byte[] fragment = new byte[40];
        System.arraycopy(d, 8, fragment, 3, 16);
        assertTrue(leaks(fragment, secrets));

        byte[] negated = new byte[64];
        System.arraycopy(nd, 0, negated, 20, 32);
        assertTrue(leaks(negated, secrets));

        assertFalse(leaks(c.signPin(CardTestSupport.DEFAULT_PIN, 0, new byte[32]), secrets),
            "a genuine signature is not flagged");
        assertFalse(leaks(c.getPub(0), secrets));
    }
}
