package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * Pins the test-side verifier to the official BIP340 vectors 0-14
 * (bitcoin/bips bip-0340/test-vectors.csv; columns: index, secret key, public key, aux_rand,
 * message, signature, result, comment). Vectors 15-18 have non-32-byte messages and are not
 * used by this card (it signs 32-byte digests only).
 */
class Bip340VerifierVectorsTest {

    static List<String[]> vectors() throws Exception {
        List<String[]> out = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new InputStreamReader(
            Bip340VerifierVectorsTest.class.getResourceAsStream("/bip340-vectors.csv"),
            StandardCharsets.UTF_8))) {
            r.readLine(); // header
            String line;
            while ((line = r.readLine()) != null) out.add(line.split(",", -1));
        }
        return out;
    }

    @Test
    void allFifteenOfficialVectors() throws Exception {
        List<String[]> v = vectors();
        assertEquals(15, v.size());
        for (String[] c : v) {
            boolean expect = c[6].equals("TRUE");
            assertEquals(expect,
                Bip340.verify(Bip340.hex(c[2]), Bip340.hex(c[4]), Bip340.hex(c[5])),
                "vector " + c[0]);
        }
    }

    @Test
    void secretKeysInVectorsMatchPublicKeys() throws Exception {
        int checked = 0;
        for (String[] c : vectors()) {
            if (c[1].isEmpty()) continue;
            assertEquals(c[2].toLowerCase(),
                Bip340.hex(Bip340.bytes32(Bip340.pubkey(new java.math.BigInteger(c[1], 16))[0])),
                "vector " + c[0]);
            checked++;
        }
        assertEquals(4, checked);
    }

    /** Anti-vacuity: a flipped bit in a valid vector must be rejected. */
    @Test
    void verifierCanFail() throws Exception {
        String[] c = vectors().get(1);
        byte[] sig = Bip340.hex(c[5]);
        assertTrue(Bip340.verify(Bip340.hex(c[2]), Bip340.hex(c[4]), sig));
        sig[40] ^= 1;
        assertFalse(Bip340.verify(Bip340.hex(c[2]), Bip340.hex(c[4]), sig));
        byte[] msg = Bip340.hex(c[4]);
        msg[0] ^= 1;
        assertFalse(Bip340.verify(Bip340.hex(c[2]), msg, Bip340.hex(c[5])));
    }
}
