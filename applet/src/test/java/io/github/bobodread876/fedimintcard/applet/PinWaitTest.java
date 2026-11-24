package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.TearHarnessTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import javacard.framework.OwnerPIN;
import javacard.security.ECPrivateKey;
import javacard.security.KeyBuilder;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

/**
 * The PIN never blocks: each wrong try makes the next VERIFY_PIN wait twice as long as the one before, 1, 2, 4, 8, 16
 * askings (each a signature's work with a key that holds no value, answered 6A98), then stretches of 16, each written
 * down as it ends. The count and the wait are persistent, counted before the PIN is compared, and a right PIN after its
 * wait sets them to nothing. Adapted from cashucard's PinWaitTest.
 */
class PinWaitTest {

    static final int CHANGE_PIN = 0x42, GET_STATE = 0x16, KEEP_HOLDING = 0x6A98;
    static final byte[] WRONG = { '9', '9', '9', '9' };

    /** A card with a PIN and a note, in a new session with no PIN given. Returns the card; the note's key is in slot 0. */
    static Card card() throws Exception {
        Card c = new Card();
        readyNote(c);
        c.powerCycle();
        return c;
    }

    static ResponseAPDU verify(Card c, byte[] pin) { return c.tx(VERIFY_PIN, 0, pin); }

    /** The askings answered 6A98 (with nothing) before the card answers otherwise, and that answer, as { askings, sw }. */
    static int[] asked(Card c, byte[] pin) {
        int n = 0;
        ResponseAPDU r;
        while ((r = verify(c, pin)).getSW() == KEEP_HOLDING) {
            assertEquals(0, r.getData().length, "keep holding, and nothing else");
            assertTrue(++n <= 100, "a wait goes down with the askings (none here is longer than 33)");
        }
        return new int[] { n, r.getSW() };
    }

    static int[] marks(Card c) throws Exception {
        byte[] m = (byte[]) c.field("pinMarks");
        return new int[] { m[0], ((m[1] & 0xFF) << 8) | (m[2] & 0xFF) };
    }

    static void setMarks(Card c, int wrong, int owed) throws Exception {
        byte[] m = (byte[]) c.field("pinMarks");
        m[0] = (byte) wrong;
        m[1] = (byte) (owed >> 8);
        m[2] = (byte) owed;
    }

    static int tries(Card c) throws Exception { return ((OwnerPIN) c.field("pin")).getTriesRemaining(); }

    static int pinState(Card c) throws Exception { return (byte) c.field("pinState"); }

    static String source(String method) throws Exception {
        String src = new String(Files.readAllBytes(SchnorrHWMathTest.mainSourceDir().resolve("FedimintCardApplet.java")), StandardCharsets.UTF_8);
        int from = src.indexOf("private void " + method + "(");
        assertTrue(from > 0, method + " is in the applet");
        return src.substring(from, src.indexOf("private ", from + 20 + method.length()));
    }

    @Test
    @DisplayName("A stranger's wrong tries strand nothing: 63C1, then waits; after the power the holder's right PIN serves its wait, opens the card, and the note is spent")
    void aStrangerStrandsNothing() throws Exception {
        Card c = new Card();
        byte[] pubX = readyNote(c);
        c.powerCycle();
        assertEquals(0x63C1, verify(c, WRONG).getSW());
        assertEquals(KEEP_HOLDING, verify(c, WRONG).getSW(), "the second try waits");
        assertEquals(0x63C2, verify(c, WRONG).getSW(), "and is counted after its wait");
        assertEquals(KEEP_HOLDING, verify(c, WRONG).getSW(), "the third waits two askings");
        c.powerCycle();
        assertEquals(1, pinState(c), "the PIN is set, never blocked");
        int[] r = asked(c, PIN);
        assertEquals(2, r[0], "two askings of wait, the one before the power not kept");
        assertEquals(0x9000, r[1], "the holder's right PIN");
        ResponseAPDU s = c.tx(SIGN, 0, MSG);
        assertEquals(0x9000, s.getSW());
        assertTrue(Bip340.verify(pubX, MSG, s.getData()), "and the note is spent");
    }

    @Test
    @DisplayName("Each wrong try in a row doubles the next try's wait: 1, 2, 4, 8 and 16 askings, then two stretches of 16 (the asking that ends a stretch answers 6A98 with no work); 63CX counts the wrong tries; the chip's counter never runs out")
    void theWaitDoubles() throws Exception {
        Card c = card();
        assertEquals(0x63C1, verify(c, WRONG).getSW(), "the first wrong try waits for nothing");
        assertArrayEquals(new int[] { 1, 1 }, marks(c));
        int[] askings = { 1, 2, 4, 8, 16, 33 };
        for (int k = 0; k < askings.length; k++) {
            int[] r = asked(c, WRONG);
            assertEquals(askings[k], r[0], "the wait before wrong try " + (k + 2));
            assertEquals(0x63C0 | (k + 2), r[1]);
            assertArrayEquals(new int[] { k + 2, 1 << (k + 1) }, marks(c), "after wrong try " + (k + 2));
            assertEquals(1, pinState(c), "set, not blocked");
            assertTrue(tries(c) >= 2, "the chip's counter refilled before each check");
        }
    }

    @Test
    @DisplayName("The count and the wait outlast the power: a card taken away mid-stretch loses that stretch's askings, never a stretch done; any other command or a SELECT between askings begins the stretch again; the right PIN after the wait sets both to nothing")
    void theWaitOutlastsThePower() throws Exception {
        Card c = card();
        setMarks(c, 6, 32);   // after the sixth wrong try in a row: 32 askings owed
        for (int k = 0; k < 10; k++) assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        c.powerCycle();
        assertArrayEquals(new int[] { 6, 32 }, marks(c), "ten askings of a stretch, then the power: nothing written");
        for (int k = 0; k < 17; k++) assertEquals(KEEP_HOLDING, verify(c, PIN).getSW(), "asking " + (k + 1));
        assertArrayEquals(new int[] { 6, 16 }, marks(c), "a stretch ended: written");
        c.powerCycle();
        assertArrayEquals(new int[] { 6, 16 }, marks(c), "and kept through the power");
        for (int k = 0; k < 5; k++) assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        assertEquals(0x9000, c.tx(GET_STATE, 0, null).getSW());
        int[] r = asked(c, PIN);
        assertEquals(16, r[0], "GET_STATE between askings: the stretch from its first");
        assertEquals(0x9000, r[1]);
        assertArrayEquals(new int[] { 0, 0 }, marks(c), "the right PIN: nothing");
        assertEquals(FedimintCardApplet.PIN_TRIES, tries(c));
        c.powerCycle();
        assertEquals(0x63C1, verify(c, WRONG).getSW(), "the next wrong try waits 1 again");
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        c.select();
        int[] again = asked(c, PIN);
        assertEquals(1, again[0], "a SELECT between askings: from the first");
        assertEquals(0x9000, again[1]);
    }

    @Test
    @DisplayName("The wait is at most 16,384 askings a try, from the fifteenth wrong try in a row on; 63CF for fifteen or more, and the count stops at 15")
    void theWaitHasACeiling() throws Exception {
        Card c = card();
        setMarks(c, 14, 0);
        assertEquals(0x63CF, verify(c, WRONG).getSW());
        assertArrayEquals(new int[] { 15, 0x4000 }, marks(c));
        setMarks(c, 15, 0);
        assertEquals(0x63CF, verify(c, WRONG).getSW(), "fifteen stays fifteen");
        assertArrayEquals(new int[] { 15, 0x4000 }, marks(c), "and the wait its ceiling");
        setMarks(c, 13, 0);
        assertEquals(0x63CE, verify(c, WRONG).getSW());
        assertArrayEquals(new int[] { 14, 0x2000 }, marks(c));
        // forty tries in a row with their waits written off: the chip's own counter never reaches 0, nothing answers 6983
        for (int i = 0; i < 40; i++) {
            setMarks(c, 15, 0);
            assertEquals(0x63CF, verify(c, WRONG).getSW(), "try " + i);
            assertTrue(tries(c) > 0);
        }
        setMarks(c, 15, 0);
        assertEquals(0x9000, verify(c, PIN).getSW(), "the right PIN, its wait served");
        assertArrayEquals(new int[] { 0, 0 }, marks(c));
    }

    @Test
    @DisplayName("The try is counted before the PIN is compared, in VERIFY_PIN and in CHANGE_PIN alike, and the wait is served before anything is read (PinWaitTearTest tears every write)")
    void theTryIsCountedFirst() throws Exception {
        String tryPin = source("tryPin");
        assertTrue(tryPin.indexOf("countTry();") > 0 && tryPin.indexOf("countTry();") < tryPin.indexOf("pin.check("), "countTry before the check");
        String count = source("countTry");
        assertTrue(count.indexOf("beginTransaction") < count.indexOf("pinMarks[PM_WRONG] = (") && count.lastIndexOf("pinMarks") < count.indexOf("commitTransaction"),
            "the count and the wait in one transaction");
        assertTrue(count.indexOf("commitTransaction") < count.indexOf("resetAndUnblock"), "then the chip's counter refilled");
        String verifyPin = source("verifyPin");
        assertTrue(verifyPin.indexOf("waitOwed(") > 0 && verifyPin.indexOf("waitOwed(") < verifyPin.indexOf("setIncomingAndReceive"), "the wait before anything is read");
        assertTrue(verifyPin.contains("tryPin("));
        assertTrue(source("changePin").contains("tryPin("), "CHANGE_PIN's old PIN through the same count");
    }

    @Test
    @DisplayName("A wrong old PIN in CHANGE_PIN counts as a try with its wait, and ends the session")
    void aWrongOldPinInChangePinCounts() throws Exception {
        Card c = card();
        assertEquals(0x9000, verify(c, PIN).getSW());
        byte[] change = new byte[8];
        System.arraycopy(WRONG, 0, change, 0, 4);
        System.arraycopy(WRONG, 0, change, 4, 4);
        assertEquals(0x63C1, c.tx(CHANGE_PIN, 0, change).getSW());
        assertArrayEquals(new int[] { 1, 1 }, marks(c));
        assertEquals(0x6A94, c.tx(SIGN, 0, MSG).getSW(), "the session is over");
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW(), "the next try owes its wait");
        assertEquals(0x9000, verify(c, PIN).getSW());
        assertArrayEquals(new int[] { 0, 0 }, marks(c));
    }

    /** Persistent writes and lasting key-data writes during `n` askings, from a card owing 32. */
    static int[] writesOfAskings(Card c, int n) throws Exception {
        setMarks(c, 6, 32);
        TearHook.count = 0;
        TearHook.keyBytes = 0;
        TearHook.trace = new ArrayList<>();
        for (int k = 0; k < n; k++) assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        int writes = TearHook.count;
        String all = String.join("\n", TearHook.trace);
        TearHook.trace = null;
        if (writes != 0) fail("an asking wrote:\n" + all);
        return new int[] { writes, TearHook.keyBytes };
    }

    @Test
    @DisplayName("An asking writes nothing persistent and needs no slot key: a card with no slot keyed serves its wait; only a stretch's end writes, the 2 bytes owed in one transaction")
    void anAskingWritesNothing() throws Exception {
        Card c = new Card();
        setPin(c);
        assertNotNull(c.field("workKeys"), "SET_PIN made the work keys");
        // (jCardSim gives the signer no key in RAM, so these askings take the RAM rounds; the next test drives both)
        for (Object k : (Object[]) c.field("privs")) assertNull(k, "no slot key exists");
        c.powerCycle();
        assertArrayEquals(new int[] { 0, 0 }, writesOfAskings(c, 16), "sixteen askings: no write");
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW(), "the stretch's end");
        String all = String.join("\n", TearHook.trace);
        TearHook.trace = null;
        assertArrayEquals(new int[] { 6, 16 }, marks(c));
        assertTrue(all.contains("commit @waitOwed"), all);
        assertFalse(all.replaceAll("(?m)^.*pinMarks\\[[12]\\].*$", "").replaceAll("(?m)^.*commit @waitOwed.*$", "").trim().length() > 0,
            "only the owed count and its commit:\n" + all);
        String work = source("work");
        assertFalse(work.contains("privs"), "the asking's work never names a slot key");
        assertTrue(work.contains("workKeys"), "it signs with the work keys");
        assertTrue(work.contains("keyInRam()"), "where the signer keeps its nonce key in RAM");
    }

    @Test
    @DisplayName("An asking signs with the work keys only where the signer keeps its nonce key in RAM; elsewhere it is SHA-256 rounds in RAM: the same 6A98s and no write, not even to key data")
    void anAskingSignsOnlyWhereTheNonceKeyIsInRam() throws Exception {
        Card c = card();
        Object schnorr = c.field("schnorr");
        java.lang.reflect.Field kr = schnorr.getClass().getDeclaredField("keyInRam"), tp = schnorr.getClass().getDeclaredField("tmpPriv");
        kr.setAccessible(true);
        tp.setAccessible(true);
        // the fallback the signer takes on such a chip: a persistent temporary key
        ECPrivateKey persistent = (ECPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
        Secp256k1.curve(persistent);
        persistent.setS(Secp256k1.SECP256K1_G, (short) 1, (short) 32);
        tp.set(schnorr, persistent);
        kr.setBoolean(schnorr, false);
        // the check is live: one SIGN on this card writes key data
        assertEquals(0x9000, verify(c, PIN).getSW());
        TearHook.keyBytes = 0;
        assertEquals(0x9000, c.tx(SIGN, 0, MSG).getSW());
        assertTrue(TearHook.keyBytes > 0, "a signature writes the persistent temporary key");
        c.powerCycle();
        assertArrayEquals(new int[] { 0, 0 }, writesOfAskings(c, 16), "sixteen askings: no write, no key data");
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        assertArrayEquals(new int[] { 6, 16 }, marks(c), "and the stretch is served");
        // control: where the nonce key is in RAM an asking signs, and the planted persistent key takes its data
        // (last: told it is in RAM, the signer clears that key after use the way it clears a RAM key)
        c.powerCycle();
        kr.setBoolean(schnorr, true);
        setMarks(c, 1, 1);
        TearHook.keyBytes = 0;
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        assertTrue(TearHook.keyBytes > 0, "an asking signs where the nonce key is in RAM");
    }
}
