package io.github.bobodread876.fedimintcard.applet;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.SecureRandom;
import java.util.Arrays;
import java.util.function.Function;

import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;

/**
 * The host's side of the sealed PIN (docs/OFFLINE-PROTOCOL.md § 9), for the tests: GET_NONCE, the PIN block and the
 * envelope, built from the spec's formulas with BigInteger arithmetic (Bip340) and the JDK's SHA-256, never from the
 * applet's code. {@link #send} lets a test speak P1 00 the PIN commands had before 4.0 (P1 = 0, the PIN as it
 * is) and sends it sealed, as a host of 4.0 does: one GET_NONCE and one envelope per tap, the same envelope again at
 * every 6A98. A P1 00 form no PIN block can carry (a length the card refuses, a P1 or P2 it refuses) is sent so the
 * card answers it as it answered the clear one: a length as Lc not 90 or 99 (6700), a P1 or P2 as P1 = 00 (6B00).
 */
public final class Sealer {
    public static final int GET_NONCE = 0x44;
    static final byte[] SEAL_LABEL = "fedimintcard/seal1".getBytes(StandardCharsets.US_ASCII);
    static final byte[] PIN_LABEL = "fedimintcard/pin1".getBytes(StandardCharsets.US_ASCII);
    static final BigInteger P = new BigInteger("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F", 16);
    static final BigInteger N = new BigInteger("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", 16);
    private static final SecureRandom RNG = new SecureRandom();

    private final Function<CommandAPDU, ResponseAPDU> card;
    /** The VERIFY_PIN envelope of this tap and the P1 00 PIN it carries, held while the card answers 6A98. */
    private CommandAPDU held;
    private byte[] heldClear;

    public Sealer(Function<CommandAPDU, ResponseAPDU> card) { this.card = card; }

    /** A new selection, a power cycle or a new GET_NONCE: the held envelope's nonce is gone. */
    public void forget() { held = null; heldClear = null; }

    /** GET_NONCE's answer: nonce (16) || PIN key (33) || binding (DER). */
    public static final class Nonce {
        public final byte[] nonce, key, der;
        Nonce(byte[] a) {
            nonce = Arrays.copyOfRange(a, 0, 16);
            key = Arrays.copyOfRange(a, 16, 49);
            der = Arrays.copyOfRange(a, 49, a.length);
        }
    }

    public ResponseAPDU getNonceRaw() {
        forget();
        return card.apply(new CommandAPDU(0xB0, GET_NONCE, 0x01, 0x00, 256));
    }

    /** GET_NONCE, or null where the card gives none. */
    public Nonce getNonce() {
        ResponseAPDU r = getNonceRaw();
        return r.getSW() == 0x9000 ? new Nonce(r.getData()) : null;
    }

    /** The PIN block: the PIN's length (1) || the PIN || zeros to 8 bytes. */
    public static byte[] block(byte[] pin) {
        byte[] b = new byte[9];
        b[0] = (byte) pin.length;
        System.arraycopy(pin, 0, b, 1, pin.length);
        return b;
    }

    public static byte[] cat(byte[]... parts) {
        int n = 0;
        for (byte[] p : parts) n += p.length;
        byte[] out = new byte[n];
        int at = 0;
        for (byte[] p : parts) { System.arraycopy(p, 0, out, at, p.length); at += p.length; }
        return out;
    }

    static BigInteger[] decompress(byte[] k) {
        if (k.length != 33 || (k[0] != 2 && k[0] != 3)) throw new IllegalArgumentException("not a compressed key");
        BigInteger[] pt = Bip340.liftX(new BigInteger(1, Arrays.copyOfRange(k, 1, 33)));
        if (pt[1].testBit(0) != (k[0] == 3)) pt = new BigInteger[] { pt[0], P.subtract(pt[1]) };
        return pt;
    }

    static byte[] sha256(byte[]... parts) {
        try {
            MessageDigest d = MessageDigest.getInstance("SHA-256");
            for (byte[] p : parts) d.update(p);
            return d.digest();
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException(e);
        }
    }

    /** block(i, m) = SHA-256(label || i || x || E || nonce || INS || m). */
    static byte[] block(int i, byte[] x, byte[] e, byte[] nonce, int ins, byte[] m) {
        return sha256(SEAL_LABEL, new byte[] { (byte) i }, x, e, nonce, new byte[] { (byte) ins }, m);
    }

    /** The envelope E || ct || tag for `clear`, sealed to the PIN key under the nonce for INS, with the sender key e. */
    public static byte[] seal(byte[] key33, byte[] nonce, int ins, byte[] clear, BigInteger e) {
        byte[] bigE = Bip340.uncompressed(Bip340.mul(e, Bip340.g()));
        byte[] x = Bip340.bytes32(Bip340.mul(e, decompress(key33))[0]);
        byte[] ct = clear.clone();
        for (int i = 1; (i - 1) * 32 < ct.length; i++) {
            byte[] b = block(i, x, bigE, nonce, ins, new byte[0]);
            for (int k = 0; k < 32 && (i - 1) * 32 + k < ct.length; k++) ct[(i - 1) * 32 + k] ^= b[k];
        }
        byte[] tag = Arrays.copyOf(block(0, x, bigE, nonce, ins, ct), 16);
        return cat(bigE, ct, tag);
    }

    public static byte[] seal(byte[] key33, byte[] nonce, int ins, byte[] clear) {
        return seal(key33, nonce, ins, clear, new BigInteger(256, RNG).mod(N.subtract(BigInteger.ONE)).add(BigInteger.ONE));
    }

    /** A fresh GET_NONCE and the command sealed under it; with no nonce to be had, 04 and zeros (the card answers 6985 or sooner). */
    public CommandAPDU sealedCommand(int ins, byte[] clear) {
        Nonce n = getNonce();
        byte[] env = n == null ? cat(new byte[] { 4 }, new byte[64 + clear.length + 16]) : seal(n.key, n.nonce, ins, clear);
        return new CommandAPDU(0xB0, ins, 0x01, 0x00, env);
    }

    private ResponseAPDU raw(int cla, int ins, int p1, int p2, byte[] data) {
        return card.apply(data == null || data.length == 0
            ? new CommandAPDU(cla, ins, p1, p2)
            : new CommandAPDU(cla, ins, p1, p2, data));
    }

    /** A command as a test speaks it; VERIFY_PIN, SET_PIN and CHANGE_PIN in their P1 00 form, sent sealed. */
    public ResponseAPDU send(int cla, int ins, int p1, int p2, byte[] data) {
        if (cla == 0x00 && ins == 0xA4 || cla == 0xB0 && ins == GET_NONCE) forget();
        if (cla != 0xB0 || ins < 0x40 || ins > 0x42) return raw(cla, ins, p1, p2, data);
        byte[] d = data == null ? new byte[0] : data;
        byte[] clear;
        if (ins == 0x42) {
            int old = p1;
            if (p2 != 0 || (old != 0 && (old < 4 || old > 8))) return raw(cla, ins, 0, p2, d);
            if (old == 0) old = (d.length & 1) != 0 ? 99 : d.length / 2;
            int young = d.length - old;
            if (old > d.length || old < 4 || old > 8 || young < 4 || young > 8) return raw(cla, ins, 1, 0, d);
            clear = cat(block(Arrays.copyOf(d, old)), block(Arrays.copyOfRange(d, old, d.length)));
        } else {
            if (p1 != 0 || p2 != 0) return raw(cla, ins, 0, p2, d);
            if (d.length < 4 || d.length > 8) return raw(cla, ins, 1, 0, d);
            clear = block(d);
        }
        if (ins == 0x40 && held != null && Arrays.equals(heldClear, d)) {
            ResponseAPDU r = card.apply(held);
            if (r.getSW() == 0x6A98) return r;
            forget();
            if (r.getSW() != 0x6985) return r;
            // the held nonce is gone (a SELECT the test made by hand): a new tap
        }
        CommandAPDU c = sealedCommand(ins, clear);
        ResponseAPDU r = card.apply(c);
        if (ins == 0x40 && r.getSW() == 0x6A98) {
            held = c;
            heldClear = d.clone();
        }
        return r;
    }
}
