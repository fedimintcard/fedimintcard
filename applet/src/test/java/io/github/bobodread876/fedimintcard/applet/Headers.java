package io.github.bobodread876.fedimintcard.applet;

import java.lang.reflect.Field;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;

/**
 * Block headers for the clock's tests, as Cashu card 3.1's tests make them (CashuAppletTest.mine/useFloor): real mainnet
 * headers for the built floor, and headers mined here for the tests' floor, which a test puts in place of the built one
 * by reflection. Also the anchor a set-up sends before SET_PIN.
 */
public final class Headers {
    private Headers() {}

    /** Real mainnet blocks 900 000, 900 200 and 900 201 (June 2025, bits 0x17023774), and the genesis block. */
    public static final byte[] H900000 = Bip340.hex("00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92");
    public static final byte[] H900200 = Bip340.hex("0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104");
    public static final long T900000 = 1749188499L;

    /** The applet's built floor, 0x17087BC0, little-endian. */
    public static final byte[] BUILT_FLOOR = { (byte) 0xC0, (byte) 0x7B, (byte) 0x08, (byte) 0x17 };
    /** The tests' floor: a target of about 2^255, half of all hashes. Stands for the built floor. */
    public static final long TEST_BITS = 0x207FFFFFL;
    /** Just over twice the tests' floor's work: the least a card with no hardest takes. Stands for a real block. */
    public static final long CLOCK_BITS = 0x203FFFFFL;
    /** The time of the anchor mined for the tests' floor: November 2023, before every header the tests send. */
    public static final long ANCHOR_TIME = 1_700_000_000L;

    public static byte[] le32(long v) {
        return new byte[] { (byte) v, (byte) (v >>> 8), (byte) (v >>> 16), (byte) (v >>> 24) };
    }

    static byte[] sha256(byte[] b) {
        try {
            return MessageDigest.getInstance("SHA-256").digest(b);
        } catch (java.security.NoSuchAlgorithmException e) {
            throw new IllegalStateException(e);
        }
    }

    public static BigInteger targetOf(long bits) {
        int exponent = (int) ((bits >>> 24) & 0xFF);
        BigInteger mantissa = BigInteger.valueOf(bits & 0x007FFFFFL);
        return exponent <= 3 ? mantissa.shiftRight(8 * (3 - exponent)) : mantissa.shiftLeft(8 * (exponent - 3));
    }

    /** An 80-byte header of that time and bits whose double SHA-256 is at or under its own target; `salt` makes another. */
    public static byte[] mine(long time, long bits, int salt) {
        BigInteger target = targetOf(bits);
        byte[] prev = sha256(("previous block " + time + "/" + salt).getBytes(StandardCharsets.US_ASCII));
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

    public static byte[] mine(long time, long bits) { return mine(time, bits, 0); }

    static Field floorField() throws ReflectiveOperationException {
        Field f = FedimintCardApplet.class.getDeclaredField("FLOOR_BITS");
        f.setAccessible(true);
        return f;
    }

    /** The applet's FLOOR_BITS (a private static final array), written in place. REFLECTION. */
    public static void useFloor(byte[] bits) {
        try {
            System.arraycopy(bits, 0, (byte[]) floorField().get(null), 0, 4);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    public static void useTheTestFloor() { useFloor(le32(TEST_BITS)); }

    public static void useTheBuiltFloor() { useFloor(BUILT_FLOOR); }

    public static boolean builtFloor() {
        try {
            return java.util.Arrays.equals(BUILT_FLOOR, (byte[]) floorField().get(null));
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static byte[] mined;

    /**
     * The header a set-up sends in SET_PIN's selection, its anchor: block 900 000 at the built floor; at the tests' floor
     * a header of CLOCK_BITS at ANCHOR_TIME, so the test's own headers of CLOCK_BITS are not weak after it.
     */
    public static synchronized byte[] anchor() {
        if (builtFloor()) return H900000;
        if (mined == null) mined = mine(ANCHOR_TIME, CLOCK_BITS);
        return mined;
    }
}
