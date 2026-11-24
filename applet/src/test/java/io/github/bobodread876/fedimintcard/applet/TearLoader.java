package io.github.bobodread876.fedimintcard.applet;

import org.objectweb.asm.ClassReader;
import org.objectweb.asm.ClassVisitor;
import org.objectweb.asm.ClassWriter;
import org.objectweb.asm.MethodVisitor;
import org.objectweb.asm.Opcodes;

import java.nio.file.Files;
import java.nio.file.Path;

/**
 * The tear harness: loads the applet's own classes (target/classes) with every array store, Util write, transaction call,
 * OwnerPIN update and key write routed through {@link TearHook}. Everything else comes from the parent loader.
 */
final class TearLoader extends ClassLoader {
    static final String HOOK = "io/github/bobodread876/fedimintcard/applet/TearHook";
    private final Path classes;

    TearLoader(ClassLoader parent, Path classes) {
        super(parent);
        this.classes = classes;
    }

    @Override
    protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
        synchronized (getClassLoadingLock(name)) {
            Path file = classes.resolve(name.replace('.', '/') + ".class");
            if (name.startsWith("io.github.bobodread876.fedimintcard.applet.") && Files.exists(file)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try {
                        byte[] b = transform(Files.readAllBytes(file));
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                if (resolve) resolveClass(c);
                return c;
            }
            return super.loadClass(name, resolve);
        }
    }

    static byte[] transform(byte[] in) {
        ClassReader r = new ClassReader(in);
        ClassWriter w = new ClassWriter(r, ClassWriter.COMPUTE_MAXS);
        final String cname = r.getClassName();
        final boolean wrap = cname.equals("io/github/bobodread876/fedimintcard/applet/FedimintCardApplet");
        r.accept(new ClassVisitor(Opcodes.ASM9, w) {
            @Override
            public void visitEnd() {
                if (wrap) {
                    // process(APDU) { try { process$orig(apdu); } catch (RuntimeException e) { TearHook.thrown(e); throw e; } }
                    MethodVisitor m = super.visitMethod(Opcodes.ACC_PUBLIC, "process", "(Ljavacard/framework/APDU;)V", null, null);
                    m.visitCode();
                    org.objectweb.asm.Label a = new org.objectweb.asm.Label(), b = new org.objectweb.asm.Label(), h = new org.objectweb.asm.Label();
                    m.visitTryCatchBlock(a, b, h, "java/lang/RuntimeException");
                    m.visitLabel(a);
                    m.visitVarInsn(Opcodes.ALOAD, 0);
                    m.visitVarInsn(Opcodes.ALOAD, 1);
                    m.visitMethodInsn(Opcodes.INVOKEVIRTUAL, cname, "process$orig", "(Ljavacard/framework/APDU;)V", false);
                    m.visitLabel(b);
                    m.visitInsn(Opcodes.RETURN);
                    m.visitLabel(h);
                    m.visitFrame(Opcodes.F_FULL, 2, new Object[] { cname, "javacard/framework/APDU" }, 1, new Object[] { "java/lang/RuntimeException" });
                    m.visitInsn(Opcodes.DUP);
                    m.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "thrown", "(Ljava/lang/Throwable;)V", false);
                    m.visitInsn(Opcodes.ATHROW);
                    m.visitMaxs(3, 2);
                    m.visitEnd();
                }
                super.visitEnd();
            }

            @Override
            public MethodVisitor visitMethod(int access, String name, String desc, String sig, String[] ex) {
                if (wrap && name.equals("process") && desc.equals("(Ljavacard/framework/APDU;)V")) name = "process$orig";
                MethodVisitor mv = super.visitMethod(access, name, desc, sig, ex);
                return new MethodVisitor(Opcodes.ASM9, mv) {
                    @Override
                    public void visitInsn(int op) {
                        if (op == Opcodes.BASTORE) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "bastore", "(Ljava/lang/Object;II)V", false);
                        } else if (op == Opcodes.SASTORE) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "sastore", "(Ljava/lang/Object;II)V", false);
                        } else {
                            super.visitInsn(op);
                        }
                    }

                    @Override
                    public void visitMethodInsn(int op, String owner, String mname, String mdesc, boolean itf) {
                        if (op == Opcodes.INVOKESTATIC && owner.equals("javacard/framework/Util")
                            && (mname.equals("arrayCopy") || mname.equals("arrayCopyNonAtomic") || mname.equals("arrayFillNonAtomic")
                                || mname.equals("setShort"))) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, mname, mdesc, false);
                            return;
                        }
                        if (op == Opcodes.INVOKESTATIC && owner.equals("javacard/framework/JCSystem")
                            && (mname.equals("beginTransaction") || mname.equals("commitTransaction") || mname.equals("abortTransaction"))) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, mname, mdesc, false);
                            return;
                        }
                        if (op == Opcodes.INVOKEVIRTUAL && owner.equals("javacard/framework/OwnerPIN")
                            && (mname.equals("check") || mname.equals("update") || mname.equals("resetAndUnblock"))) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, mname,
                                "(Ljavacard/framework/OwnerPIN;" + mdesc.substring(1), false);
                            return;
                        }
                        if (owner.equals("javacard/security/ECPublicKey") && mname.equals("setW")) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "setW", "(Ljavacard/security/ECPublicKey;" + mdesc.substring(1), false);
                            return;
                        }
                        // the signer's own key object, written at every signature and point addition (counted, never torn)
                        if (owner.equals("javacard/security/ECPrivateKey") && (mname.equals("setS") || mname.equals("setG"))) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "priv" + mname, "(Ljavacard/security/ECPrivateKey;" + mdesc.substring(1), false);
                            return;
                        }
                        if (owner.equals("javacard/security/KeyPair") && mname.equals("genKeyPair")) {
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, HOOK, "genKeyPair", "(Ljavacard/security/KeyPair;)V", false);
                            return;
                        }
                        super.visitMethodInsn(op, owner, mname, mdesc, itf);
                    }
                };
            }
        }, 0);
        return w.toByteArray();
    }
}
