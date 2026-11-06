package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.assertSw;
import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import javacard.security.AESKey;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * STATE_KEY (INS 0x13) answers HMAC-SHA256(card secret, "fedimintcard/state/v1"): the key a till
 * encrypts the card's state with. It is gated exactly like SIGN and never changes for a card.
 */
class StateKeyTest {

    static {
        // jCardSim's RandomData is a fixed-seed generator unless told otherwise, which would give
        // every simulated card the same secret.
        System.setProperty("com.licel.jcardsim.randomdata.secure", "1");
    }

    static byte[] stateKey(CardTestSupport c) {
        ResponseAPDU r = c.tx(CardTestSupport.STATE_KEY, 0, null);
        assertSw(0x9000, r);
        return r.getData();
    }

    @Test
    void refusedBeforeAPinIsSet() {
        CardTestSupport c = new CardTestSupport();
        ResponseAPDU r = c.tx(CardTestSupport.STATE_KEY, 0, null);
        assertSw(0x6984, r);
        assertEquals(0, r.getData().length);
    }

    @Test
    void refusedWithoutThisSelectionsVerify() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        assertSw(0x6A94, c.tx(CardTestSupport.STATE_KEY, 0, null));
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        stateKey(c);
        c.select();
        assertSw(0x6A94, c.tx(CardTestSupport.STATE_KEY, 0, null), "a new selection needs the PIN again");
    }

    @Test
    void stableAcrossSelectionsAndPowerCycles() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        byte[] first = stateKey(c);
        assertEquals(32, first.length);
        c.powerCycle();
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        assertArrayEquals(first, stateKey(c));
    }

    @Test
    void differsBetweenCards() {
        CardTestSupport a = new CardTestSupport(), b = new CardTestSupport();
        for (CardTestSupport c : new CardTestSupport[] { a, b }) {
            c.setPin(CardTestSupport.DEFAULT_PIN);
            c.verifyPin(CardTestSupport.DEFAULT_PIN);
        }
        assertFalse(java.util.Arrays.equals(stateKey(a), stateKey(b)));
    }

    @Test
    void isTheHmacOfTheCardSecret() throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        byte[] secret = new byte[32];
        ((AESKey) c.field("secret")).getKey(secret, (short) 0);
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(secret, "HmacSHA256"));
        byte[] want = mac.doFinal("fedimintcard/state/v1".getBytes(StandardCharsets.US_ASCII));
        assertArrayEquals(want, stateKey(c));
    }

    @Test
    void pinChangeKeepsTheKey() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        byte[] before = stateKey(c);
        assertSw(0x9000, c.changePinRaw("12345678".getBytes(StandardCharsets.US_ASCII)));
        c.select();
        c.verifyPin("5678".getBytes(StandardCharsets.US_ASCII));
        assertArrayEquals(before, stateKey(c));
    }

    @Test
    void nonZeroP1P2IsRefused() {
        CardTestSupport c = new CardTestSupport();
        c.setPin(CardTestSupport.DEFAULT_PIN);
        c.verifyPin(CardTestSupport.DEFAULT_PIN);
        assertSw(0x6B00, c.tx(CardTestSupport.CLA, CardTestSupport.STATE_KEY, 1, 0, null));
        assertSw(0x6B00, c.tx(CardTestSupport.CLA, CardTestSupport.STATE_KEY, 0, 1, null));
    }
}
