package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Deque;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import org.junit.jupiter.api.Test;

/**
 * JavaCard Classic never collects: every {@code new} on a command path is a permanent EEPROM/Flash
 * leak that fills the card after a few hundred taps. So the applet allocates once, at install
 * time — in a constructor, {@code install()}, or (SchnorrHW only) {@code init()}, which the applet
 * constructor calls — and a command path may not allocate at all.
 *
 * <p>This is a source scan, not a bytecode check: it reads the three main-source files by path
 * (relative to the applet module), strips comments and string/char literals, tracks method bodies
 * by brace depth, and reports every {@code new} with its line number and enclosing method.
 */
class NoAllocationTest {

    /**
     * Install-time methods, per file, where {@code new} is allowed. Everything else in these files
     * is a command path or a helper it calls, and must be allocation-free: the applet's
     * {@code genKey}, for instance, only re-parameterises key objects and allocates nothing.
     *
     * <p>{@code <static>} covers a static field initialiser. On Java Card the class is loaded and
     * its static fields are built when the applet is installed, never on a command path, and the
     * domain-parameter constants ({@code P}, {@code A}, {@code B}, {@code N}, {@code G}) are
     * exactly that: {@code new byte[32]} once, at install.
     */
    static final Map<String, Set<String>> ALLOWED = Map.of(
        "FedimintCardApplet.java", Set.of("FedimintCardApplet", "install", "<static>"),
        "SchnorrHW.java", Set.of("SchnorrHW", "init"),
        "Opener.java", Set.of("Opener", "install"));

    static final List<String> SOURCES = List.of(
        "FedimintCardApplet.java", "SchnorrHW.java", "Opener.java");

    /** Where the main sources live, relative to this module ({@code applet/}). */
    static Path sourceDir() {
        Path here = Paths.get("").toAbsolutePath();
        Path p = here.resolve("src/main/java/io/github/bobodread876/fedimintcard/applet");
        if (Files.isDirectory(p)) return p;
        // Surefire runs with the module as working directory; if it ever does not, walk up.
        for (Path dir = here; dir != null; dir = dir.getParent()) {
            Path cand = dir.resolve("applet/src/main/java/io/github/bobodread876/fedimintcard/applet");
            if (Files.isDirectory(cand)) return cand;
        }
        throw new IllegalStateException("cannot find the applet sources from " + here);
    }

    @Test
    void noAllocationOutsideInstallTime() throws IOException {
        List<String> offences = new ArrayList<>();
        int seen = 0, allowedSeen = 0;
        for (String file : SOURCES) {
            String src = Files.readString(sourceDir().resolve(file), StandardCharsets.UTF_8);
            Set<String> allowed = ALLOWED.get(file);
            assertNotNull(allowed, "every scanned file needs an allow-list entry: " + file);
            for (Alloc a : allocations(src)) {
                seen++;
                if (allowed.contains(a.method)) {
                    allowedSeen++;
                } else {
                    offences.add(file + ":" + a.line + ": `new` in " + describe(a.method)
                        + " (allowed only in " + allowed + ")");
                }
            }
        }
        // Anti-vacuity: the files do allocate, and the scan found the install-time allocations.
        assertTrue(seen > 0, "the scan found no `new` at all; check sourceDir()");
        assertTrue(allowedSeen > 0, "none of the install-time allocations were recognised; check ALLOWED");
        assertTrue(offences.isEmpty(), "allocations outside install time:\n  " + String.join("\n  ", offences));
    }

    /** The scan is not vacuous: a planted `new` in a command path is caught with its line number. */
    @Test
    void catchesAPlantedAllocation() {
        String synthetic = String.join("\n",
            "final class Planted {",                              // 1
            "    private final byte[] b = new byte[2];",           // 2  field initialiser (<static>)
            "    private final byte[] a;",                        // 3
            "    Planted() {",                                    // 4
            "        a = new byte[4];",                           // 5  allowed (constructor)
            "    }",                                              // 6
            "    public static void install(byte[] b) {",         // 7
            "        new Planted();",                             // 8  allowed (install)
            "    }",                                              // 9
            "    void command() {",                               // 10
            "        // new byte[1] in a comment is not code",    // 11
            "        String s = \"new byte[2] in a string\";",    // 12
            "        byte[] leak = new byte[3];",                 // 13 offending
            "    }",                                              // 14
            "    void helper() { byte[] h = new byte[1]; }",      // 15 offending
            "}");                                                 // 16
        List<String> located = new ArrayList<>();
        for (Alloc a : allocations(synthetic)) located.add(a.line + ":" + a.method);
        assertEquals(List.of("2:<static>", "5:Planted", "8:install", "13:command", "15:helper"), located);
        // The fixture is held to its own allow-list, not the applet's: `<static>` is legal in the
        // applet, and borrowing that list here would let a planted allocation slip past.
        Set<String> allowed = Set.of("Planted", "install", "<static>");
        List<String> offending = new ArrayList<>();
        for (Alloc a : allocations(synthetic)) if (!allowed.contains(a.method)) offending.add(a.toString());
        assertEquals(List.of("13:command", "15:helper"), offending);
    }

    /** Comment and literal stripping must not hide a real allocation either. */
    @Test
    void realSourcesAreActuallyParsed() throws IOException {
        String src = Files.readString(sourceDir().resolve("FedimintCardApplet.java"), StandardCharsets.UTF_8);
        assertTrue(src.contains("Holds up to"), "the javadoc text is there to be stripped");
        assertFalse(strip(src).contains("Holds up to"), "javadoc is stripped");
        assertEquals("<static>", allocations("class A { byte[] a = new byte[1]; }").get(0).method);
    }

    static String describe(String m) {
        return m.equals("<static>") ? "a field initialiser" : m + "()";
    }

    // ── the scanner ──────────────────────────────────────────────────────────

    static final class Alloc {
        final int line;
        final String method;

        Alloc(int line, String method) {
            this.line = line;
            this.method = method;
        }

        @Override public String toString() { return line + ":" + method; }
    }

    /**
     * Every {@code new} in the stripped source, with its line and the method whose body encloses
     * it (the innermost method at any nesting depth); a field initialiser is reported as
     * {@code <static>}. Anonymous classes inside a method body are reported under that method,
     * which is the strict reading: they are still constructed on that path.
     */
    static List<Alloc> allocations(String source) {
        String s = strip(source);
        int[] lineOf = new int[s.length() + 1];
        for (int i = 0, line = 1; i < s.length(); i++) {
            lineOf[i] = line;
            if (s.charAt(i) == '\n') line++;
        }
        Map<Integer, String> methodAtBrace = methodHeaders(s);
        List<Alloc> out = new ArrayList<>();
        Deque<String> enclosing = new ArrayDeque<>();
        for (int i = 0; i < s.length(); i++) {
            char ch = s.charAt(i);
            if (ch == '{') {
                String name = methodAtBrace.get(i);
                if (name != null) enclosing.push(name);
                else enclosing.push("");
            } else if (ch == '}') {
                if (!enclosing.isEmpty()) enclosing.pop();
            }
            // Not `else if`: a one-line method body is `{ new X(); }`, and the `new` shares its
            // index with the brace that names the method it belongs to.
            if (ch == 'n' && s.startsWith("new ", i)
                    && (i == 0 || !Character.isJavaIdentifierPart(s.charAt(i - 1)))) {
                String method = "<static>";
                for (String m : enclosing) {
                    if (!m.isEmpty()) { method = m; break; }
                }
                out.add(new Alloc(lineOf[i], method));
            }
        }
        return out;
    }

    /**
     * Maps the index of each {@code {} that opens a method body to that method's name.
     *
     * <p>Scanning is driven by {@code )}, not {@code (}: a parameter list may itself contain
     * parentheses (an anonymous class argument, a cast), so the closing paren is where a method
     * header reliably ends. For each {@code )} that follows a call-shaped token, the parameter
     * list is back-matched to its {@code (}, the name is read left of that, and a brace next
     * closes a method body. A {@code )} whose left neighbour cannot be a call — an operator, a
     * separator, a parenthesis — is a grouping paren or a cast and is skipped.
     */
    static Map<Integer, String> methodHeaders(String s) {
        Map<Integer, String> out = new TreeMap<>();
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) != ')') continue;
            int nameEnd = i;
            while (nameEnd > 0 && Character.isWhitespace(s.charAt(nameEnd - 1))) nameEnd--;
            if (nameEnd == 0) continue;
            char prev = s.charAt(nameEnd - 1);
            // A call or a declaration: an identifier, a generic close, an array bracket, or an
            // empty parameter list "()" whose last token is the paren itself.
            if (!Character.isJavaIdentifierPart(prev) && prev != '>' && prev != ']' && prev != '(') continue;
            int open = matchBrace(s, i, ')', '(');
            String name = nameBefore(s, open);
            if (KEYWORDS.contains(name)) continue;
            // `new Planted(...)` is a construction, not a header: the token before the name is
            // `new`, and treating it as a header would swallow the enclosing method.
            if (nameBefore(s, nameOffset(s, open)).equals("new")) continue;
            int brace = i + 1;
            while (brace < s.length() && Character.isWhitespace(s.charAt(brace))) brace++;
            if (brace < s.length() && s.charAt(brace) == '{') {
                out.put(brace, name);
                i = brace;
            }
        }
        return out;
    }

    /** The identifier immediately left of the {@code (} at {@code open}, or "" if there is none. */
    static String nameBefore(String s, int open) {
        int end = nameEnd(s, open);
        int start = end;
        while (start > 0 && Character.isJavaIdentifierPart(s.charAt(start - 1))) start--;
        return start == end ? "" : s.substring(start, end);
    }

    /** Index just past the identifier that ends before the {@code (} at {@code open}. */
    static int nameOffset(String s, int open) {
        return nameEnd(s, open);
    }

    static int nameEnd(String s, int open) {
        int end = open;
        while (end > 0 && Character.isWhitespace(s.charAt(end - 1))) end--;
        return end;
    }

    static final Set<String> KEYWORDS = Set.of(
        "if", "for", "while", "switch", "catch", "synchronized", "return", "new", "throw", "else",
        "do", "try", "assert", "case", "instanceof");

    /** Index of the brace closing the one at {@code from}, or {@code from} if unbalanced. */
    static int matchBrace(String s, int from, char open, char close) {
        if (close == '(') return matchBack(s, from);
        int d = 0;
        for (int i = from; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == open) d++;
            else if (c == close && --d == 0) return i;
        }
        return from;
    }

    /** Index of the paren matching the {@code )} at {@code from}, scanning backwards. */
    static int matchBack(String s, int from) {
        int d = 0;
        for (int i = from; i >= 0; i--) {
            char c = s.charAt(i);
            if (c == ')') d++;
            else if (c == '(' && --d == 0) return i;
        }
        return from;
    }

    /** Replaces comments and string/char literals with spaces, keeping newlines and offsets. */
    static String strip(String src) {
        StringBuilder b = new StringBuilder(src.length());
        int i = 0, n = src.length();
        while (i < n) {
            char c = src.charAt(i);
            if (c == '/' && i + 1 < n && src.charAt(i + 1) == '/') {
                while (i < n && src.charAt(i) != '\n') { b.append(' '); i++; }
            } else if (c == '/' && i + 1 < n && src.charAt(i + 1) == '*') {
                b.append("  ");
                i += 2;
                while (i < n && !(src.charAt(i) == '*' && i + 1 < n && src.charAt(i + 1) == '/')) {
                    b.append(src.charAt(i) == '\n' ? '\n' : ' ');
                    i++;
                }
                if (i < n) { b.append("  "); i += 2; }
            } else if (c == '"' || c == '\'') {
                char q = c;
                b.append(' ');
                i++;
                while (i < n && src.charAt(i) != q) {
                    if (src.charAt(i) == '\\' && i + 1 < n) { b.append("  "); i += 2; continue; }
                    b.append(src.charAt(i) == '\n' ? '\n' : ' ');
                    i++;
                }
                if (i < n) { b.append(' '); i++; }
            } else {
                b.append(c);
                i++;
            }
        }
        return b.toString();
    }
}
