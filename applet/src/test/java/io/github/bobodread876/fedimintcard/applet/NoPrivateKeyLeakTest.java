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

    @Test
    void sweepsLeakNothing() throws Exception {
        CardTestSupport c = new CardTestSupport();
        List<byte[]> secrets = new ArrayList<>();
        List<byte[]> genWire = new ArrayList<>();
        for (int s = 0; s < 8; s++) {
            genWire.add(wire(c.tx(CardTestSupport.GEN_KEY, s, null)));
            BigInteger d = c.scalar(s);
            secrets.add(Bip340.bytes32(d));
            secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
        }
        for (byte[] w : genWire) {
            assertEquals(34, w.length);
            assertFalse(leaks(w, secrets), "GEN_KEY response leaked the scalar");
        }
        int checked = 0;
        // Full INS sweep with a small matrix.
        for (int ins = 0; ins < 256; ins++) {
            for (int p1 : new int[] { 0, 8 }) {
              for (int p2 : new int[] { 0, 1 }) {
                for (int len : new int[] { 0, 32 }) {
                    ResponseAPDU r = c.tx(0xB0, ins, p1, p2, new byte[len]);
                    if (ins == 0x10 && p2 == 1 && p1 == 0 && r.getSW() == 0x9000) {
                        BigInteger d = c.scalar(0);
                        secrets.add(Bip340.bytes32(d));
                        secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
                    }
                    assertFalse(leaks(wire(r), secrets), "INS " + ins + " P1 " + p1 + " len " + len);
                    checked++;
                }
              }
            }
        }
        // Full matrix for the three real commands.
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
                            BigInteger d = c.scalar(p1);
                            secrets.add(Bip340.bytes32(d));
                            secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
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

    /** Same sweep on a fresh card with empty slots; keys that appear mid-sweep are checked as they appear. */
    @Test
    void sweepOnFreshCardLeaksNothing() throws Exception {
        CardTestSupport c = new CardTestSupport();
        int checked = 0;
        for (int ins = 0; ins < 256; ins++) {
            for (int p1 : new int[] { 0, 1, 8 }) {
              for (int p2 : new int[] { 0, 1 }) {
                for (int len : new int[] { 0, 32 }) {
                    ResponseAPDU r = c.tx(0xB0, ins, p1, p2, new byte[len]);
                    List<byte[]> secrets = new ArrayList<>();
                    for (int s = 0; s < 8; s++) {
                        try {
                            BigInteger d = c.scalar(s);
                            if (d.signum() == 0) continue;
                            secrets.add(Bip340.bytes32(d));
                            secrets.add(Bip340.bytes32(Bip340.N.subtract(d)));
                        } catch (RuntimeException e) {
                            // slot has no key yet
                        }
                    }
                    assertFalse(leaks(wire(r), secrets), "INS " + ins + " P1 " + p1 + " len " + len);
                    checked++;
                }
              }
            }
        }
        assertTrue(checked > 1500);
    }

    /** The only INS values the applet answers (with CLA B0) are exactly 0x10, 0x11 and 0x12. */
    @Test
    void onlyThreeCommandsExist() {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        List<Integer> known = new ArrayList<>();
        for (int ins = 0; ins < 256; ins++) {
            if (c.tx(0xB0, ins, 0, 0, null).getSW() != 0x6D00) known.add(ins);
        }
        assertEquals(List.of(0x10, 0x11, 0x12), known);
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

        assertFalse(leaks(c.sign(0, new byte[32]), secrets), "a genuine signature is not flagged");
        assertFalse(leaks(c.getPub(0), secrets));
    }
}
