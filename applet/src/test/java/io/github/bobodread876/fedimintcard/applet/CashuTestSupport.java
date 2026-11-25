package io.github.bobodread876.fedimintcard.applet;

/**
 * The curve and byte helpers of cashucard's CashuAppletTest (be4b129), copied unchanged for the signer tests ported
 * from that repository (SignatureCheckTest, SignHardeningTest, ChipEnginesTest, SchnorrHWMathTest), which import
 * them statically as they imported CashuAppletTest's. Reference arithmetic in BigInteger, never the card's code.
 */
final class CashuTestSupport {
    private CashuTestSupport() {}

    static byte[] concat(byte[]... parts) {
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        for (byte[] p : parts) out.write(p, 0, p.length);
        return out.toByteArray();
    }

    static byte[] u32(long v) { byte[] b = new byte[4]; putUint32(b, 0, v); return b; }

    static byte[] unsigned32(java.math.BigInteger n) {
        byte[] b = n.toByteArray();
        byte[] out = new byte[32];
        int len = Math.min(b.length, 32);
        System.arraycopy(b, b.length - len, out, 32 - len, len);
        return out;
    }

    /** A point as 65 bytes, 04 || x || y. */
    static byte[] point65(java.math.BigInteger[] p) { return concat(new byte[] { 4 }, unsigned32(p[0]), unsigned32(p[1])); }

    /** A key made up by whoever holds `k`: its compressed point. */
    static byte[] keyOf(java.math.BigInteger k) { return compressed(ecMulTest(k, SECP_GX, SECP_GY)); }

    /** A compressed secp256k1 point, 02/03 || X. */
    static byte[] compressed(java.math.BigInteger[] w) {
        byte[] c = new byte[33];
        c[0] = (byte) (w[1].testBit(0) ? 3 : 2);
        System.arraycopy(unsigned32(w[0]), 0, c, 1, 32);
        return c;
    }

    static void putUint32(byte[] buf, int off, long v) {
        buf[off]     = (byte)((v >> 24) & 0xFF);
        buf[off + 1] = (byte)((v >> 16) & 0xFF);
        buf[off + 2] = (byte)((v >> 8)  & 0xFF);
        buf[off + 3] = (byte)( v        & 0xFF);
    }

    static String toHex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte v : b) sb.append(String.format("%02x", v & 0xFF));
        return sb.toString();
    }

    static byte[] sha256(byte[] in) {
        try { return java.security.MessageDigest.getInstance("SHA-256").digest(in); }
        catch (java.security.NoSuchAlgorithmException e) { throw new IllegalStateException(e); }
    }

    static byte[] hexToBytes(String hex) {
        int len = hex.length();
        byte[] out = new byte[len / 2];
        for (int i = 0; i < len; i += 2) {
            out[i / 2] = (byte) Integer.parseInt(hex.substring(i, i + 2), 16);
        }
        return out;
    }

    static long readUint32(byte[] buf, int offset) {
        return ((long)(buf[offset]     & 0xFF) << 24)
             | ((long)(buf[offset + 1] & 0xFF) << 16)
             | ((long)(buf[offset + 2] & 0xFF) << 8)
             |  (long)(buf[offset + 3] & 0xFF);
    }

    static final java.math.BigInteger SECP_P = new java.math.BigInteger(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F", 16);

    static final java.math.BigInteger SECP_N = new java.math.BigInteger(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", 16);

    static final java.math.BigInteger SECP_GX = new java.math.BigInteger(
        "79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798", 16);

    static final java.math.BigInteger SECP_GY = new java.math.BigInteger(
        "483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8", 16);

    /**
     * BIP-340 Schnorr verify.
     * sig = R.x (32) || s (32)
     */
    static boolean schnorrVerify(byte[] pubX, byte[] msg, byte[] sig)
            throws java.security.NoSuchAlgorithmException {
        java.math.BigInteger p = SECP_P;
        java.math.BigInteger n = SECP_N;

        java.math.BigInteger r = new java.math.BigInteger(1,
            java.util.Arrays.copyOfRange(sig, 0, 32));
        java.math.BigInteger s = new java.math.BigInteger(1,
            java.util.Arrays.copyOfRange(sig, 32, 64));

        if (r.compareTo(p) >= 0) return false;
        if (s.compareTo(n) >= 0) return false;

        // P = lift_x(pubX) — even-y point
        java.math.BigInteger[] P = liftX(new java.math.BigInteger(1, pubX));
        if (P == null) return false;

        // e = tagged_hash("BIP0340/challenge", bytes(r) || bytes(P.x) || msg) mod n
        byte[] rBytes  = toBytes32Test(r);
        byte[] PxBytes = toBytes32Test(P[0]);
        byte[] challengeInput = new byte[96];
        System.arraycopy(rBytes,  0, challengeInput,  0, 32);
        System.arraycopy(PxBytes, 0, challengeInput, 32, 32);
        System.arraycopy(msg,     0, challengeInput, 64, 32);
        java.math.BigInteger e = new java.math.BigInteger(1,
            taggedHashTest("BIP0340/challenge", challengeInput)).mod(n);

        // R = s*G - e*P  (subtract = add negated point: -P = (P.x, p - P.y))
        java.math.BigInteger[] sG  = ecMulTest(s,  SECP_GX, SECP_GY);
        java.math.BigInteger[] eP  = ecMulTest(e,  P[0],    P[1]);
        if (sG == null || eP == null) return false;

        // Negate eP: (eP.x, p - eP.y)
        java.math.BigInteger[] negEP = { eP[0], p.subtract(eP[1]) };
        java.math.BigInteger[] R = ecAddTest(sG[0], sG[1], negEP[0], negEP[1]);
        if (R == null) return false;

        // R.y must be even, R.x must equal r
        if (R[1].testBit(0)) return false;
        return R[0].equals(r);
    }

    /** lift_x: find the even-y point on secp256k1 with the given x-coordinate. */
    static java.math.BigInteger[] liftX(java.math.BigInteger x) {
        java.math.BigInteger p = SECP_P;
        if (x.compareTo(p) >= 0) return null;
        java.math.BigInteger rhs = x.modPow(java.math.BigInteger.valueOf(3), p)
            .add(java.math.BigInteger.valueOf(7)).mod(p);
        java.math.BigInteger y = rhs.modPow(p.add(java.math.BigInteger.ONE)
            .divide(java.math.BigInteger.valueOf(4)), p);
        // Verify it's actually a square root
        if (!y.modPow(java.math.BigInteger.TWO, p).equals(rhs)) return null;
        // Choose even y
        if (y.testBit(0)) y = p.subtract(y);
        return new java.math.BigInteger[]{ x, y };
    }

    /** Scalar multiplication: k * (x,y) using double-and-add. */
    static java.math.BigInteger[] ecMulTest(java.math.BigInteger k,
                                             java.math.BigInteger x,
                                             java.math.BigInteger y) {
        java.math.BigInteger[] R = null;
        java.math.BigInteger[] P = { x, y };
        k = k.mod(SECP_N);
        while (k.signum() > 0) {
            if (k.testBit(0)) {
                R = (R == null) ? new java.math.BigInteger[]{ P[0], P[1] }
                                : ecAddTest(R[0], R[1], P[0], P[1]);
            }
            P = ecAddTest(P[0], P[1], P[0], P[1]);
            k = k.shiftRight(1);
        }
        return R;
    }

    /** EC point addition / doubling on secp256k1. Returns null for point at infinity. */
    static java.math.BigInteger[] ecAddTest(java.math.BigInteger x1,
                                             java.math.BigInteger y1,
                                             java.math.BigInteger x2,
                                             java.math.BigInteger y2) {
        java.math.BigInteger p  = SECP_P;
        java.math.BigInteger p2 = p.subtract(java.math.BigInteger.TWO);
        java.math.BigInteger lambda;
        if (x1.equals(x2)) {
            if (!y1.equals(y2)) return null; // point at infinity
            // doubling
            java.math.BigInteger num = java.math.BigInteger.valueOf(3)
                .multiply(x1.modPow(java.math.BigInteger.TWO, p)).mod(p);
            java.math.BigInteger den = java.math.BigInteger.valueOf(2).multiply(y1).mod(p);
            lambda = num.multiply(den.modPow(p2, p)).mod(p);
        } else {
            java.math.BigInteger num = y2.subtract(y1).mod(p);
            java.math.BigInteger den = x2.subtract(x1).mod(p);
            lambda = num.multiply(den.modPow(p2, p)).mod(p);
        }
        java.math.BigInteger x3 = lambda.modPow(java.math.BigInteger.TWO, p)
            .subtract(x1).subtract(x2).mod(p);
        java.math.BigInteger y3 = lambda.multiply(x1.subtract(x3)).subtract(y1).mod(p);
        if (x3.signum() < 0) x3 = x3.add(p);
        if (y3.signum() < 0) y3 = y3.add(p);
        return new java.math.BigInteger[]{ x3, y3 };
    }

    /** BIP-340 tagged hash: SHA256(SHA256(tag) || SHA256(tag) || msg) */
    static byte[] taggedHashTest(String tag, byte[] msg)
            throws java.security.NoSuchAlgorithmException {
        java.security.MessageDigest sha =
            java.security.MessageDigest.getInstance("SHA-256");
        byte[] tagHash = sha.digest(
            tag.getBytes(java.nio.charset.StandardCharsets.UTF_8));
        sha.reset();
        sha.update(tagHash);
        sha.update(tagHash);
        sha.update(msg);
        return sha.digest();
    }

    /** BigInteger → big-endian 32-byte array (zero-padded / sign-stripped). */
    static byte[] toBytes32Test(java.math.BigInteger n) {
        byte[] b = n.toByteArray();
        if (b.length == 32) return b;
        byte[] out = new byte[32];
        if (b.length > 32) {
            System.arraycopy(b, b.length - 32, out, 0, 32);
        } else {
            System.arraycopy(b, 0, out, 32 - b.length, b.length);
        }
        return out;
    }
}
