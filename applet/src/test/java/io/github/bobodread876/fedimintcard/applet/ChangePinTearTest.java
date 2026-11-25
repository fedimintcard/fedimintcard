package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.PinWaitTest.*;
import static io.github.bobodread876.fedimintcard.applet.TearHarnessTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * CHANGE_PIN torn after each of its writes in turn, adapted from Cashu card 3.1's: there a torn change on a blocked card
 * was the danger. Here no card blocks, the state byte is already set, and CHANGE_PIN's old PIN is a counted try, so a
 * torn change must leave exactly one PIN that opens the card, the old one before OwnerPIN's update and the new one after
 * it, with the marks the try left: never made, counted with its wait, or cleared. A wrong old PIN torn anywhere was
 * counted with its wait or never made, and the old PIN still opens the card once the wait is served.
 */
class ChangePinTearTest {

    static final byte[] NEW = { '5', '6', '7', '8', '9', '0' };

    static byte[] data(byte[] old) {
        byte[] d = Arrays.copyOf(old, old.length + NEW.length);
        System.arraycopy(NEW, 0, d, old.length, NEW.length);
        return d;
    }

    /** A card with PIN, in a session that has it. */
    static Card session() throws Exception {
        Card c = new Card();
        setPin(c);
        c.select();
        assertEquals(0x9000, verify(c, PIN).getSW());
        return c;
    }

    static List<String> sweep(byte[] old, int[] total) throws Exception {
        boolean right = Arrays.equals(old, PIN);
        Card c = session();
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        assertEquals(right ? 0x9000 : 0x63C1, c.tx(CHANGE_PIN, old.length, data(old)).getSW());
        List<String> trace = TearHook.trace;
        TearHook.trace = null;
        total[0] = TearHook.count;
        int update = Integer.MAX_VALUE;
        for (int i = 0; i < trace.size(); i++) if (trace.get(i).contains("pin.update")) update = i + 1;
        String all = String.join("\n", trace);
        assertTrue(all.indexOf("commit @countTry") < all.indexOf("pin.check"), "counted before the compare:\n" + all);
        if (right) assertTrue(update < Integer.MAX_VALUE, all);
        List<String> wrong = new ArrayList<>();
        for (int k = 1; k <= total[0]; k++) {
            Card t = session();
            TearHook.count = 0;
            TearHook.target = k;
            int r = t.tx(CHANGE_PIN, old.length, data(old)).getSW();
            String at = TearHook.tornAt;
            TearHook.target = 0;
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            if (r == 0x9000) wrong.add("write " + k + ": a torn CHANGE_PIN answered 9000");
            t.powerCycle();
            int[] m = marks(t);
            if (!(m[0] == 0 && m[1] == 0) && !(m[0] == 1 && m[1] == 1)) wrong.add("write " + k + " (" + at + "): marks " + Arrays.toString(m));
            if (pinState(t) != 1) wrong.add("write " + k + ": PIN state " + pinState(t));
            if (tries(t) == 0) wrong.add("write " + k + ": the chip's counter ran out");
            for (byte b : (byte[]) t.field("newPin")) if (b != 0) { wrong.add("write " + k + ": the new PIN outlived the selection"); break; }
            byte[] opens = k >= update ? NEW : PIN, other = k >= update ? PIN : NEW;
            int[] a = asked(t, opens);
            if (a[1] != 0x9000 || a[0] != m[1]) wrong.add("write " + k + " (" + at + "): " + new String(opens) + " after " + a[0] + " askings answered " + Integer.toHexString(a[1]));
            t.select();
            int o = verify(t, other).getSW();
            if (o != 0x63C1) wrong.add("write " + k + ": " + new String(other) + " answered " + Integer.toHexString(o));
        }
        return wrong;
    }

    @Test
    void aChangeTornAnywhereLeavesExactlyOnePinThatOpens() throws Exception {
        int[] total = new int[1];
        assertEquals(List.of(), sweep(PIN, total));
        System.out.println("CHANGE_PIN (right) tear sweep: " + total[0] + " writes, each torn");
        assertTrue(total[0] >= 5, "the copy, the count, the commit, the check, the clear, the update: " + total[0]);
    }

    @Test
    void aWrongOldPinTornAnywhereWasCountedWithItsWaitOrNeverMade() throws Exception {
        int[] total = new int[1];
        assertEquals(List.of(), sweep(WRONG, total));
        System.out.println("CHANGE_PIN (wrong) tear sweep: " + total[0] + " writes, each torn");
        assertTrue(total[0] >= 3, "the copy, the count, the commit, the check: " + total[0]);
    }
}
