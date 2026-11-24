package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.JCSystem;
import javacard.framework.OwnerPIN;
import javacard.framework.Util;
import javacard.security.ECPublicKey;
import javacard.security.KeyPair;

import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Collections;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * The tear harness, its runtime side of the tearing harness. The applet's bytecode is rewritten by {@link TearLoader} so
 * that every store into an array, every Util copy/fill/setShort, every transaction call, every OwnerPIN update and every
 * key write goes through here. A store into one of the applet's PERSISTENT arrays is an "event"; the harness tears the
 * card (an Error the applet cannot catch) right after event number `target`.
 *
 * Unlike jCardSim, this models a real chip's transactions: a snapshot at beginTransaction, rolled back on abort, on a
 * tear inside the transaction, and on any exception that leaves process() with a transaction open; and the
 * NonAtomic Util writes made inside a transaction are NOT rolled back (they never take part in one on a chip).
 * A NonAtomic copy or fill of more than one byte outside a transaction can be torn half way (two events).
 */
public final class TearHook {

    /** The card leaving the field. An Error: the applet's catch (Exception)/(RuntimeException) blocks do not catch it. */
    public static final class Tear extends Error {
        public Tear() { super("tear", null, false, false); }
    }

    public static int count, target;
    /** The card torn right after the next commit of a transaction begun in a method whose name (and line) starts so. */
    public static String tearAfterCommit;
    public static boolean splitNonAtomic = true;
    public static List<String> trace;
    public static boolean tornInTx;
    public static String tornAt;
    /** Commands that left process() by an exception with a transaction open: rolled back here as a chip would. */
    public static final List<String> leftOpen = new ArrayList<>();
    /** NonAtomic writes made inside a transaction (they survive a rollback on a chip). */
    public static final List<String> nonAtomicInTx = new ArrayList<>();

    static Object applet;
    static final Set<Object> persistent = Collections.newSetFromMap(new IdentityHashMap<>());
    static final Map<Object, String> names = new IdentityHashMap<>();
    static final List<Field> refFields = new ArrayList<>();
    static OwnerPIN pinObj;
    static ECPublicKey registryKeyObj;
    static final Set<Object> keyPairs = Collections.newSetFromMap(new IdentityHashMap<>());

    public static int depth;
    static Map<Object, Object> snapArrays;
    static Map<Field, Object> snapRefs;
    static Object[] snapPin;
    static byte[] snapRegistryW;
    static final List<Object[]> nonAtomicLog = new ArrayList<>();
    public static String beganAt;

    /** The applet's CLEAR_ON_DESELECT arrays: RAM on a chip, never torn and never rolled back. */
    static final String[] TRANSIENT = { "tmp", "x" };

    /** The last exception, not an ISOException, that left process(): what jCardSim answers 6F00 for. */
    public static Throwable lastThrown;

    public static void thrown(Throwable t) {
        if (!(t instanceof javacard.framework.ISOException)) lastThrown = t;
    }

    public static void attach(Object a) {
        applet = a;
        refresh();
    }

    /** The persistent arrays, as the applet's fields hold them now (some are reserved later, at SET_CARD or CERT_WRITE). */
    public static void refresh() {
        persistent.clear();
        names.clear();
        refFields.clear();
        keyPairs.clear();
        try {
            for (Field f : applet.getClass().getDeclaredFields()) {
                f.setAccessible(true);
                boolean stat = Modifier.isStatic(f.getModifiers());
                Object v = f.get(stat ? null : applet);
                if (!stat) refFields.add(f);
                if (v == null) continue;
                boolean tr = false;
                for (String t : TRANSIENT) if (t.equals(f.getName())) tr = true;
                if ((v instanceof byte[] || v instanceof short[] || v instanceof boolean[]) && !tr && !stat) {
                    persistent.add(v);
                    names.put(v, f.getName());
                }
                if (v instanceof OwnerPIN) pinObj = (OwnerPIN) v;
                if (f.getName().equals("registryKey")) registryKeyObj = (ECPublicKey) v;
                if (v instanceof KeyPair) keyPairs.add(v);
            }
        } catch (IllegalAccessException e) {
            throw new IllegalStateException(e);
        }
    }

    public static void reset() {
        count = 0;
        target = 0;
        tornInTx = false;
        tornAt = null;
        depth = 0;
        snapArrays = null;
        nonAtomicLog.clear();
    }

    static String where() {
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            if (e.getClassName().startsWith("io.github.bobodread876.fedimintcard.applet.") && !e.getClassName().endsWith("TearHook")) {
                return e.getMethodName() + ":" + e.getLineNumber();
            }
        }
        return "?";
    }

    // ---- what each transaction writes (writes, bytes, arrays, 64-byte pages), the largest seen per site (method:line) ----
    static final java.util.regex.Pattern TXW = java.util.regex.Pattern.compile("(?:(?:arrayCopy|arrayCopyNonAtomic|arrayFillNonAtomic)(?:\\(in tx\\))? |setShort )?(\\w+)\\[(\\d+)(?:\\+(\\d+))?\\]");
    static int txWrites, txBytes;
    static final java.util.Set<String> txArrays = new java.util.TreeSet<>(), txPages = new java.util.TreeSet<>();
    static final java.util.List<String> txLines = new ArrayList<>();
    /** site -> { writes, bytes, arrays, pages } at its largest (by bytes), and the writes of that one. */
    public static final java.util.Map<String, int[]> TX_MAX = new java.util.TreeMap<>();
    public static final java.util.Map<String, String> TX_MAX_WHAT = new java.util.TreeMap<>();
    /**
     * A chip's commit buffer, modelled: a transaction begun in a method whose site (method:line) starts with `fullAt` throws
     * TransactionException (BUFFER_FULL) at the write that takes its logged bytes past `fullBytes`, as a chip's does when the
     * buffer is full. Null: no limit, as jCardSim has none.
     */
    public static String fullAt;
    public static int fullBytes;
    /** A per-write overhead the chip charges in its commit buffer (cost = bytes + fullOverhead * writes). */
    public static int fullOverhead;
    /** Every transaction site that was refused (BUFFER_FULL) since last cleared. */
    public static final java.util.Set<String> fullHit = new java.util.TreeSet<>();
    static void txNote(String what) {
        if (depth <= 0) return;
        txWrites++;
        int len = 1;
        String arr = what;
        int off = 0;
        if (what.startsWith("pin.update")) { arr = "OwnerPIN"; len = 10; }
        else if (what.startsWith("registryKey.setW")) { arr = "registryKey"; len = 65; }
        else if (what.startsWith("genKeyPair")) { arr = "keyPair"; len = 97; }
        else if (what.startsWith("pin.")) { arr = "OwnerPIN.tries"; len = 1; }
        else {
            java.util.regex.Matcher m = TXW.matcher(what);
            if (m.find()) {
                arr = m.group(1); off = Integer.parseInt(m.group(2));
                len = m.group(3) != null ? Integer.parseInt(m.group(3)) : what.startsWith("setShort") ? 2 : 1;
            }
        }
        txBytes += len;
        txArrays.add(arr);
        for (int pg = off / 64; pg <= (off + Math.max(len, 1) - 1) / 64; pg++) txPages.add(arr + ":" + pg);
        txLines.add(what);
        if (fullAt != null && beganAt != null && beganAt.startsWith(fullAt) && txBytes + fullOverhead * txWrites > fullBytes) {
            fullHit.add(beganAt);
            javacard.framework.TransactionException.throwIt(javacard.framework.TransactionException.BUFFER_FULL);
        }
    }
    static void txBegin() { txWrites = 0; txBytes = 0; txArrays.clear(); txPages.clear(); txLines.clear(); }
    static void txEnd(String site) {
        int[] v = { txWrites, txBytes, txArrays.size(), txPages.size() };
        int[] old = TX_MAX.get(site);
        if (old == null || v[1] > old[1] || (v[1] == old[1] && v[0] > old[0])) {
            TX_MAX.put(site, v);
            TX_MAX_WHAT.put(site, String.valueOf(txLines));
        }
    }

    static void ev(String what) {
        txNote(what);
        count++;
        if (trace != null) trace.add(count + (depth > 0 ? " [tx] " : " ") + what + " @" + where());
        if (target > 0 && count == target) {
            tornInTx = depth > 0;
            tornAt = what + " @" + where() + (tornInTx ? " (in a transaction)" : "");
            throw new Tear();
        }
    }

    static boolean p(Object a) { return a != null && persistent.contains(a); }

    // ---- array stores ----
    public static void bastore(Object a, int i, int v) {
        if (a instanceof byte[]) ((byte[]) a)[i] = (byte) v; else ((boolean[]) a)[i] = v != 0;
        if (p(a)) ev(names.get(a) + "[" + i + "]=" + v);
    }

    public static void sastore(Object a, int i, int v) {
        ((short[]) a)[i] = (short) v;
        if (p(a)) ev(names.get(a) + "[" + i + "]=" + v);
    }

    // ---- Util ----
    public static short arrayCopy(byte[] s, short so, byte[] d, short dO, short len) {
        short r = Util.arrayCopy(s, so, d, dO, len);
        if (p(d)) ev("arrayCopy " + names.get(d) + "[" + dO + "+" + len + "]");
        return r;
    }

    public static short arrayCopyNonAtomic(byte[] s, short so, byte[] d, short dO, short len) {
        if (!p(d)) return Util.arrayCopyNonAtomic(s, so, d, dO, len);
        if (depth > 0) {
            short r = Util.arrayCopyNonAtomic(s, so, d, dO, len);
            logNonAtomic(d, dO, len, "arrayCopyNonAtomic");
            ev("arrayCopyNonAtomic(in tx) " + names.get(d) + "[" + dO + "+" + len + "]");
            return r;
        }
        if (splitNonAtomic && len > 1) {
            byte[] src = new byte[len];
            System.arraycopy(s, so, src, 0, len);
            short h = (short) (len / 2);
            System.arraycopy(src, 0, d, dO, h);
            ev("arrayCopyNonAtomic HALF " + names.get(d) + "[" + dO + "+" + h + "/" + len + "]");
            System.arraycopy(src, h, d, dO + h, len - h);
        } else {
            Util.arrayCopyNonAtomic(s, so, d, dO, len);
        }
        ev("arrayCopyNonAtomic " + names.get(d) + "[" + dO + "+" + len + "]");
        return (short) (dO + len);
    }

    public static short arrayFillNonAtomic(byte[] d, short o, short len, byte v) {
        if (!p(d)) return Util.arrayFillNonAtomic(d, o, len, v);
        if (depth > 0) {
            short r = Util.arrayFillNonAtomic(d, o, len, v);
            logNonAtomic(d, o, len, "arrayFillNonAtomic");
            ev("arrayFillNonAtomic(in tx) " + names.get(d) + "[" + o + "+" + len + "]");
            return r;
        }
        if (splitNonAtomic && len > 1) {
            short h = (short) (len / 2);
            Util.arrayFillNonAtomic(d, o, h, v);
            ev("arrayFillNonAtomic HALF " + names.get(d) + "[" + o + "+" + h + "/" + len + "]");
            Util.arrayFillNonAtomic(d, (short) (o + h), (short) (len - h), v);
        } else {
            Util.arrayFillNonAtomic(d, o, len, v);
        }
        ev("arrayFillNonAtomic " + names.get(d) + "[" + o + "+" + len + "]");
        return (short) (o + len);
    }

    public static short setShort(byte[] d, short o, short v) {
        short r = Util.setShort(d, o, v);
        if (p(d)) ev("setShort " + names.get(d) + "[" + o + "]");
        return r;
    }

    static void logNonAtomic(byte[] d, short o, short len, String what) {
        byte[] after = new byte[len];
        System.arraycopy(d, o, after, 0, len);
        nonAtomicLog.add(new Object[] { d, (int) o, after });
        String s = what + " " + names.get(d) + "[" + o + "+" + len + "] @" + where();
        if (!nonAtomicInTx.contains(s)) nonAtomicInTx.add(s);
    }

    // ---- transactions ----
    public static void beginTransaction() {
        JCSystem.beginTransaction();
        depth = 1;
        beganAt = where();
        snapshot();
        txBegin();
    }

    public static void commitTransaction() {
        JCSystem.commitTransaction();
        txEnd(beganAt);
        depth = 0;
        snapArrays = null;
        nonAtomicLog.clear();
        ev("commit @" + beganAt);
        if (tearAfterCommit != null && beganAt.startsWith(tearAfterCommit)) {
            tearAfterCommit = null;
            tornInTx = false;
            tornAt = "after the commit @" + beganAt;
            throw new Tear();
        }
    }

    public static void abortTransaction() {
        JCSystem.abortTransaction();
        rollback();
    }

    static void snapshot() {
        refresh();
        snapArrays = new IdentityHashMap<>();
        for (Object a : persistent) {
            if (a instanceof byte[]) snapArrays.put(a, ((byte[]) a).clone());
            else if (a instanceof short[]) snapArrays.put(a, ((short[]) a).clone());
            else snapArrays.put(a, ((boolean[]) a).clone());
        }
        snapRefs = new java.util.HashMap<>();
        try {
            for (Field f : refFields) snapRefs.put(f, f.get(applet));
        } catch (IllegalAccessException e) {
            throw new IllegalStateException(e);
        }
        snapPin = pinState(pinObj);
        snapRegistryW = null;
        try {
            if (registryKeyObj != null && registryKeyObj.isInitialized()) {
                byte[] w = new byte[65];
                registryKeyObj.getW(w, (short) 0);
                snapRegistryW = w;
            }
        } catch (RuntimeException e) {
            snapRegistryW = null;
        }
        nonAtomicLog.clear();
    }

    /** What a chip does on abort, on a tear in a transaction, and on an exception out of process() with one open. */
    public static void rollback() {
        if (snapArrays == null) { depth = 0; return; }
        for (Map.Entry<Object, Object> e : snapArrays.entrySet()) {
            Object live = e.getKey(), copy = e.getValue();
            if (live instanceof byte[]) System.arraycopy(copy, 0, live, 0, ((byte[]) copy).length);
            else if (live instanceof short[]) System.arraycopy(copy, 0, live, 0, ((short[]) copy).length);
            else System.arraycopy(copy, 0, live, 0, ((boolean[]) copy).length);
        }
        try {
            for (Map.Entry<Field, Object> e : snapRefs.entrySet()) {
                if (e.getKey().get(applet) != e.getValue()) e.getKey().set(applet, e.getValue());
            }
        } catch (IllegalAccessException e) {
            throw new IllegalStateException(e);
        }
        if (snapRegistryW != null) registryKeyObj.setW(snapRegistryW, (short) 0, (short) 65);
        // OwnerPIN's value rolls back (update takes part in a transaction); its try counter does not (check never does)
        restorePinValue(pinObj, snapPin);
        // NonAtomic writes never took part
        for (Object[] w : nonAtomicLog) {
            byte[] after = (byte[]) w[2];
            System.arraycopy(after, 0, w[0], (Integer) w[1], after.length);
        }
        nonAtomicLog.clear();
        snapArrays = null;
        depth = 0;
        refresh();
    }

    static Object[] pinState(OwnerPIN pin) {
        if (pin == null) return null;
        try {
            Field v = OwnerPIN.class.getDeclaredField("pinValue"), s = OwnerPIN.class.getDeclaredField("pinSize");
            v.setAccessible(true);
            s.setAccessible(true);
            return new Object[] { ((byte[]) v.get(pin)).clone(), s.get(pin) };
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    static void restorePinValue(OwnerPIN pin, Object[] st) {
        if (pin == null || st == null) return;
        try {
            Field v = OwnerPIN.class.getDeclaredField("pinValue"), s = OwnerPIN.class.getDeclaredField("pinSize");
            v.setAccessible(true);
            s.setAccessible(true);
            byte[] live = (byte[]) v.get(pin);
            System.arraycopy((byte[]) st[0], 0, live, 0, live.length);
            s.set(pin, st[1]);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    // ---- OwnerPIN ----
    public static boolean check(OwnerPIN pin, byte[] b, short o, byte l) {
        boolean r = pin.check(b, o, l);
        ev("pin.check -> " + r);
        return r;
    }

    public static void update(OwnerPIN pin, byte[] b, short o, byte l) {
        pin.update(b, o, l);
        ev("pin.update");
    }

    public static void resetAndUnblock(OwnerPIN pin) {
        pin.resetAndUnblock();
        ev("pin.resetAndUnblock");
    }

    // ---- keys ----
    public static void setW(ECPublicKey k, byte[] b, short o, short l) {
        k.setW(b, o, l);
        if (k == registryKeyObj) ev("registryKey.setW" + (depth > 0 ? " (key data: on a chip it may not take part in the transaction)" : ""));
    }

    /** Bytes written into private key objects (key data that lasts on a chip, unless the key is transient) since the count was last set to 0. */
    public static int keyBytes, keyCalls;
    public static void privsetS(javacard.security.ECPrivateKey k, byte[] b, short o, short l) { k.setS(b, o, l); lasting(k, l); }
    public static void privsetG(javacard.security.ECPrivateKey k, byte[] b, short o, short l) { k.setG(b, o, l); lasting(k, l); }
    /** A write of `l` bytes into a key object, counted where the key's data lasts (a key built persistent; a transient one's is RAM). */
    static void lasting(javacard.security.ECPrivateKey k, short l) {
        if (k.getType() == javacard.security.KeyBuilder.TYPE_EC_FP_PRIVATE) { keyBytes += l; keyCalls++; }
    }

    public static void genKeyPair(KeyPair kp) {
        kp.genKeyPair();
        if (keyPairs.contains(kp)) ev("genKeyPair" + (depth > 0 ? " (key data: on a chip it may not take part in the transaction)" : ""));
    }
}
