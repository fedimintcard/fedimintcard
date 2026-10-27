package io.github.bobodread876.fedimintcard.applet;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Arrays;

/** Test-only pure-Java BIP340 verifier and secp256k1 math. Independent of the applet code. */
public final class Bip340 {
    public static final BigInteger P = new BigInteger(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F", 16);
    public static final BigInteger N = new BigInteger(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", 16);
    private static final BigInteger[] G = {
        new BigInteger("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798", 16),
        new BigInteger("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8", 16) };

    private Bip340() {}

    /** Point as {x, y}; null is infinity. */
    static BigInteger[] add(BigInteger[] a, BigInteger[] b) {
        if (a == null) return b;
        if (b == null) return a;
        BigInteger lam;
        if (a[0].equals(b[0])) {
            if (!a[1].equals(b[1]) || a[1].signum() == 0) return null;
            lam = a[0].pow(2).multiply(BigInteger.valueOf(3))
                .multiply(a[1].shiftLeft(1).modInverse(P)).mod(P);
        } else {
            lam = b[1].subtract(a[1]).multiply(b[0].subtract(a[0]).modInverse(P)).mod(P);
        }
        BigInteger x = lam.pow(2).subtract(a[0]).subtract(b[0]).mod(P);
        BigInteger y = lam.multiply(a[0].subtract(x)).subtract(a[1]).mod(P);
        return new BigInteger[] { x, y };
    }

    static BigInteger[] mul(BigInteger k, BigInteger[] pt) {
        BigInteger[] r = null, q = pt;
        for (int i = 0; i < k.bitLength(); i++) {
            if (k.testBit(i)) r = add(r, q);
            q = add(q, q);
        }
        return r;
    }

    public static BigInteger[] g() { return G; }

    /** d*G. */
    public static BigInteger[] pubkey(BigInteger d) { return mul(d.mod(N), G); }

    public static byte[] bytes32(BigInteger v) {
        byte[] t = v.toByteArray();
        byte[] out = new byte[32];
        int n = Math.min(t.length, 32);
        System.arraycopy(t, t.length - n, out, 32 - n, n);
        return out;
    }

    /** 04||X||Y. */
    public static byte[] uncompressed(BigInteger[] pt) {
        byte[] out = new byte[65];
        out[0] = 4;
        System.arraycopy(bytes32(pt[0]), 0, out, 1, 32);
        System.arraycopy(bytes32(pt[1]), 0, out, 33, 32);
        return out;
    }

    static BigInteger[] liftX(BigInteger x) {
        if (x.compareTo(P) >= 0) return null;
        BigInteger c = x.pow(3).add(BigInteger.valueOf(7)).mod(P);
        BigInteger y = c.modPow(P.add(BigInteger.ONE).shiftRight(2), P);
        if (!y.pow(2).mod(P).equals(c)) return null;
        if (y.testBit(0)) y = P.subtract(y);
        return new BigInteger[] { x, y };
    }

    static byte[] taggedHash(String tag, byte[]... parts) {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] th = md.digest(tag.getBytes(StandardCharsets.UTF_8));
            md.reset();
            md.update(th);
            md.update(th);
            for (byte[] p : parts) md.update(p);
            return md.digest();
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    /** BIP340 verify: 32-byte x-only key, message of any length, 64-byte signature. */
    public static boolean verify(byte[] pubX, byte[] msg, byte[] sig) {
        if (pubX.length != 32 || sig.length != 64) return false;
        BigInteger[] pt = liftX(new BigInteger(1, pubX));
        if (pt == null) return false;
        BigInteger r = new BigInteger(1, Arrays.copyOfRange(sig, 0, 32));
        BigInteger s = new BigInteger(1, Arrays.copyOfRange(sig, 32, 64));
        if (r.compareTo(P) >= 0 || s.compareTo(N) >= 0) return false;
        BigInteger e = new BigInteger(1,
            taggedHash("BIP0340/challenge", bytes32(r), pubX, msg)).mod(N);
        BigInteger[] negEP = mul(N.subtract(e), pt);
        BigInteger[] rr = add(mul(s, G), negEP);
        return rr != null && !rr[1].testBit(0) && rr[0].equals(r);
    }

    public static byte[] hex(String h) {
        byte[] b = new byte[h.length() / 2];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) Integer.parseInt(h.substring(2 * i, 2 * i + 2), 16);
        }
        return b;
    }

    public static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }
}
