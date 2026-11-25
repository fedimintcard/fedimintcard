package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.PinWaitTest.*;
import static io.github.bobodread876.fedimintcard.applet.TearHarnessTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * VERIFY_PIN torn after each of its writes in turn, with a wrong PIN and with the right one, and a stretch's end torn:
 * every torn try was counted with its wait or never made, the count and the wait are never half written, and the right
 * PIN opens the card once the wait it then owes is served.
 */
class PinWaitTearTest {

    /** The marks the card may hold after a torn try from `w0` wrong tries and nothing owed. */
    static List<int[]> allowed(int w0, boolean right) {
        List<int[]> ok = new ArrayList<>();
        ok.add(new int[] { w0, 0 });                                            // never made
        ok.add(new int[] { Math.min(w0 + 1, 15), w0 < 14 ? 1 << w0 : 0x4000 }); // counted, with its wait
        if (right) ok.add(new int[] { 0, 0 });                                  // made, and cleared
        return ok;
    }

    /** Tears VERIFY_PIN after each write; returns what went wrong, and the count of writes in `total[0]`. */
    static List<String> sweep(int w0, byte[] sent, int[] total) throws Exception {
        boolean right = Arrays.equals(sent, PIN);
        Card c = card();
        setMarks(c, w0, 0);
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        int sw = verify(c, sent).getSW();
        List<String> trace = TearHook.trace;
        TearHook.trace = null;
        total[0] = TearHook.count;
        assertEquals(right ? 0x9000 : 0x63C0 | Math.min(w0 + 1, 15), sw);
        String all = String.join("\n", trace);
        assertTrue(all.contains("commit @countTry"), "the try is counted in a transaction:\n" + all);
        assertTrue(all.indexOf("commit @countTry") < all.indexOf("pin.check"), "before the compare:\n" + all);
        List<String> wrong = new ArrayList<>();
        for (int k = 1; k <= total[0]; k++) {
            Card t = card();
            setMarks(t, w0, 0);
            TearHook.count = 0;
            TearHook.target = k;
            int r = verify(t, sent).getSW();
            String at = TearHook.tornAt;
            TearHook.target = 0;
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            if (!trace.get(k - 1).endsWith(at.replace(" (in a transaction)", ""))) wrong.add("torn at " + at + ", told " + trace.get(k - 1));
            if (r == 0x9000) wrong.add("write " + k + ": a torn VERIFY_PIN answered 9000");
            t.powerCycle();
            int[] m = marks(t);
            boolean fits = false;
            for (int[] a : allowed(w0, right)) fits |= Arrays.equals(a, m);
            if (!fits) wrong.add("write " + k + " (" + at + "): marks " + Arrays.toString(m));
            if (tries(t) == 0) wrong.add("write " + k + ": the chip's counter ran out");
            if (pinState(t) != 1) wrong.add("write " + k + ": PIN state " + pinState(t));
            if (m[1] > 64) setMarks(t, m[0], m[1] = 0);   // the cap's wait, written off here: PinWaitTest serves long waits
            int[] a = asked(t, PIN);
            if (a[1] != 0x9000 || a[0] != m[1]) wrong.add("write " + k + ": marks " + Arrays.toString(m) + ", then " + a[0] + " askings and " + Integer.toHexString(a[1]));
            if (!Arrays.equals(new int[] { 0, 0 }, marks(t))) wrong.add("write " + k + ": the right PIN left marks " + Arrays.toString(marks(t)));
        }
        return wrong;
    }

    @Test
    void aWrongTryTornAnywhereWasCountedWithItsWaitOrNeverMade() throws Exception {
        int[] total = new int[1];
        assertEquals(List.of(), sweep(3, WRONG, total));
        System.out.println("VERIFY_PIN (wrong) tear sweep: " + total[0] + " writes, each torn");
        assertTrue(total[0] >= 4, "the count, the wait, the commit and the check: " + total[0]);
        assertEquals(List.of(), sweep(15, WRONG, total), "at the cap");
    }

    @Test
    void aRightTryTornAnywhereWasCountedOrClearedOrNeverMade() throws Exception {
        int[] total = new int[1];
        assertEquals(List.of(), sweep(3, PIN, total));
        System.out.println("VERIFY_PIN (right) tear sweep: " + total[0] + " writes, each torn");
        assertTrue(total[0] >= 5, "the count, the wait, the commit, the check and the clear: " + total[0]);
        assertEquals(List.of(), sweep(0, PIN, total), "a first try");
    }

    @Test
    void aStretchsEndTornLosesAtMostThatStretch() throws Exception {
        // the untorn stretch end, counted
        Card c = card();
        setMarks(c, 6, 32);
        for (int k = 0; k < 16; k++) assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        assertEquals(KEEP_HOLDING, verify(c, PIN).getSW());
        int total = TearHook.count;
        String all = String.join("\n", TearHook.trace);
        TearHook.trace = null;
        assertTrue(total >= 2 && all.contains("commit @waitOwed"), all);
        List<String> wrong = new ArrayList<>();
        for (int k = 1; k <= total; k++) {
            Card t = card();
            setMarks(t, 6, 32);
            for (int i = 0; i < 16; i++) assertEquals(KEEP_HOLDING, verify(t, PIN).getSW());
            TearHook.count = 0;
            TearHook.target = k;
            verify(t, PIN);
            TearHook.target = 0;
            if (TearHook.tornAt == null) { wrong.add("write " + k + " was not reached"); continue; }
            t.powerCycle();
            int[] m = marks(t);
            int[] a = asked(t, PIN);
            if (Arrays.equals(new int[] { 6, 32 }, m)) { if (a[0] != 33) wrong.add("write " + k + ": 32 owed, then " + a[0] + " askings"); }
            else if (Arrays.equals(new int[] { 6, 16 }, m)) { if (a[0] != 16) wrong.add("write " + k + ": 16 owed, then " + a[0] + " askings"); }
            else wrong.add("write " + k + " (" + TearHook.tornAt + "): marks " + Arrays.toString(m));
            if (a[1] != 0x9000) wrong.add("write " + k + ": the right PIN after the wait answered " + Integer.toHexString(a[1]));
        }
        assertEquals(List.of(), wrong);
    }
}
