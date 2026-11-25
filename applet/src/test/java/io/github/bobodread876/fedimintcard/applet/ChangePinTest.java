package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.SetPinTest.cat;
import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/** CHANGE_PIN's P1: the old PIN's length (4 to 8), or 0 for two halves of the same length. */
class ChangePinTest {

    static byte[] pin(String s) { return s.getBytes(java.nio.charset.StandardCharsets.US_ASCII); }

    static CardTestSupport card(byte[] p) {
        CardTestSupport c = new CardTestSupport();
        c.setPin(p);
        c.select();
        c.verifyPin(p);
        return c;
    }

    @Test
    void p1GivesTheOldPinsLength() {
        String[][] cases = { { "1234", "567890" }, { "123456", "7890" }, { "12345678", "4321" }, { "1234", "87654321" } };
        for (String[] k : cases) {
            CardTestSupport c = card(pin(k[0]));
            assertSw(0x9000, c.tx(CLA, CHANGE_PIN, k[0].length(), 0, cat(pin(k[0]), pin(k[1]))), k[0] + " to " + k[1]);
            assertSw(0x6A94, c.tx(STATE_KEY, 0, null), "the change ends the session");
            assertSw(0x63C1, c.verifyPinRaw(pin(k[0])), "the old PIN is gone");
            c.verifyPin(pin(k[1]));
        }
    }

    @Test
    void p1ZeroKeepsEqualHalves() {
        CardTestSupport c = card(pin("123456"));
        assertSw(0x9000, c.tx(CLA, CHANGE_PIN, 0, 0, pin("123456654321")));
        c.verifyPin(pin("654321"));
        assertSw(0x6700, c.tx(CLA, CHANGE_PIN, 0, 0, pin("654321123")), "an odd Lc with P1 0");
    }

    @Test
    void aBadP1OrLengthIsRefusedAndCostsNoTry() throws Exception {
        CardTestSupport c = card(pin("1234"));
        byte[] eight = pin("12345678");
        for (int p1 : new int[] { 1, 3, 9, 0x80, 0xFF }) assertSw(0x6B00, c.tx(CLA, CHANGE_PIN, p1, 0, eight), "P1 " + p1);
        assertSw(0x6B00, c.tx(CLA, CHANGE_PIN, 4, 1, eight), "P2");
        assertSw(0x6700, c.tx(CLA, CHANGE_PIN, 4, 0, pin("1234567")), "a new PIN of 3");
        assertSw(0x6700, c.tx(CLA, CHANGE_PIN, 4, 0, pin("1234123456789")), "a new PIN of 9");
        assertSw(0x6700, c.tx(CLA, CHANGE_PIN, 8, 0, pin("12345678")), "no new PIN");
        assertSw(0x6700, c.tx(CLA, CHANGE_PIN, 4, 0, new byte[200]), "a long field");
        assertEquals(0, ((byte[]) c.field("pinMarks"))[0], "none of them was a try");
        assertSw(0x9000, c.tx(STATE_KEY, 0, null), "and none ended the session");
    }

    @Test
    void aWrongOldPinCountsAsATryWithItsWait() throws Exception {
        CardTestSupport c = card(pin("1234"));
        assertSw(0x63C1, c.tx(CLA, CHANGE_PIN, 4, 0, pin("9999567890")));
        byte[] m = (byte[]) c.field("pinMarks");
        assertEquals(1, m[0], "counted");
        assertEquals(1, m[2], "the next try owes one asking");
        assertSw(0x6A94, c.tx(CLA, CHANGE_PIN, 4, 0, pin("1234567890")), "the session is over");
        assertSw(0x6A98, c.verifyPinRaw(pin("1234")), "the wait");
        assertSw(0x9000, c.verifyPinRaw(pin("1234")));
        assertSw(0x9000, c.tx(CLA, CHANGE_PIN, 4, 0, pin("1234567890")));
        c.verifyPin(pin("567890"));
    }
}
