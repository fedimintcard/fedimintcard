package io.github.bobodread876.fedimintcard.applet;

import java.io.ByteArrayOutputStream;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.AlgorithmParameters;
import java.security.KeyFactory;
import java.security.PrivateKey;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECParameterSpec;
import java.security.spec.ECPrivateKeySpec;

/**
 * The registry's note signature for tests (DESIGN § 6): ECDSA P-256 with SHA-256, DER, over
 * "fedimintcard/note1" (18) || the slot's x-only key (32) || SET_NOTE's 56 bytes (amount || federation signature).
 * The test registry key is P-256's generator, whose private key is 1: {@link CardTestSupport#TEST_REGISTRY_KEY}.
 */
public final class Registry {
    private Registry() {}

    public static final byte[] NOTE_LABEL = "fedimintcard/note1".getBytes(StandardCharsets.US_ASCII);

    /** The private key of TEST_REGISTRY_KEY (the generator): 1. A test key, never a deployment key. */
    public static final PrivateKey TEST_KEY = privateKey(BigInteger.ONE);

    public static PrivateKey privateKey(BigInteger d) {
        try {
            AlgorithmParameters p = AlgorithmParameters.getInstance("EC");
            p.init(new ECGenParameterSpec("secp256r1"));
            ECParameterSpec spec = p.getParameterSpec(ECParameterSpec.class);
            return KeyFactory.getInstance("EC").generatePrivate(new ECPrivateKeySpec(d, spec));
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    /** The 106-byte message: label || slot x-only key || note data (56). */
    public static byte[] noteMessage(byte[] slotX, byte[] note) {
        if (slotX.length != 32 || note.length != 56) throw new IllegalArgumentException();
        ByteArrayOutputStream m = new ByteArrayOutputStream();
        m.writeBytes(NOTE_LABEL);
        m.writeBytes(slotX);
        m.writeBytes(note);
        return m.toByteArray();
    }

    /** A DER signature over msg under key. */
    public static byte[] sign(PrivateKey key, byte[] msg) {
        try {
            Signature s = Signature.getInstance("SHA256withECDSA");
            s.initSign(key);
            s.update(msg);
            return s.sign();
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    /** SET_NOTE's data as the registry vouches for it: the note (56) || DER signature under key. */
    public static byte[] signed(PrivateKey key, byte[] slotX, byte[] note) {
        byte[] sig = sign(key, noteMessage(slotX, note));
        byte[] out = new byte[56 + sig.length];
        System.arraycopy(note, 0, out, 0, 56);
        System.arraycopy(sig, 0, out, 56, sig.length);
        return out;
    }

    /** The same, under the test registry key. */
    public static byte[] signed(byte[] slotX, byte[] note) { return signed(TEST_KEY, slotX, note); }
}
