package ghostslot;

import java.lang.instrument.Instrumentation;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.Executor;

// runs inside a Fabric client, started by Loader. the outcome goes to the
// result file so the proxy can report it. member names are Fabric intermediary,
// which is what a production Fabric client runs with
public final class Agent {
    private static final String MINECRAFT = "net.minecraft.class_310";

    private Agent() {}

    public static void run(String address, String resultFile, Instrumentation inst) {
        Path result = Path.of(resultFile);
        try {
            Class<?> mcClass = null;
            for (Class<?> c : inst.getAllLoadedClasses()) {
                if (c.getName().equals(MINECRAFT)) {
                    mcClass = c;
                    break;
                }
            }
            if (mcClass == null) {
                throw new IllegalStateException("class " + MINECRAFT + " not found, is this a Fabric client?");
            }
            ClassLoader cl = mcClass.getClassLoader();
            Object mc = mcClass.getMethod("method_1551").invoke(null);
            ((Executor) mc).execute(() -> {
                try {
                    String note = bypassTunnel(cl);
                    join(cl, mc, address);
                    write(result, note == null ? "ok" : "ok:" + note);
                } catch (Throwable t) {
                    write(result, describe(t));
                }
            });
        } catch (Throwable t) {
            write(result, describe(t));
        }
    }

    private static void join(ClassLoader cl, Object mc, String address) throws Exception {
        Class<?> mcClass = mc.getClass();
        if ((boolean) mcClass.getMethod("method_1542").invoke(mc)) {
            throw new IllegalStateException("the client is in a singleplayer world, leave it first");
        }
        Object world = field(mcClass, "field_1687").get(mc);
        if (world != null) {
            Object handler = mcClass.getMethod("method_1562").invoke(mc);
            if (handler != null) {
                Class<?> text = Class.forName("net.minecraft.class_2561", false, cl);
                Object reason = text.getMethod("method_43471", String.class)
                        .invoke(null, "multiplayer.status.quitting");
                Object connection = handler.getClass().getMethod("method_2872").invoke(handler);
                connection.getClass().getMethod("method_10747", text).invoke(connection, reason);
            }
            mcClass.getMethod("method_18099").invoke(mc);
        }

        Object title = Class.forName("net.minecraft.class_442", false, cl).getConstructor().newInstance();
        Object parsed = Class.forName("net.minecraft.class_639", false, cl)
                .getMethod("method_2950", String.class)
                .invoke(null, address);
        Object info = Class.forName("net.minecraft.class_642", false, cl)
                .getConstructor(String.class, String.class, boolean.class)
                .newInstance("ghostslot", address, false);

        // 1.19.x takes (Screen, MinecraftClient, ServerAddress, ServerInfo), later
        // versions append flags such as quickPlay that default to false/null
        Method connect = null;
        for (Method m : Class.forName("net.minecraft.class_412", false, cl).getMethods()) {
            if (m.getName().equals("method_36877") && m.getParameterCount() >= 4) {
                connect = m;
                break;
            }
        }
        if (connect == null) {
            throw new IllegalStateException("ConnectScreen.connect not found in this version");
        }
        Object[] callArgs = new Object[connect.getParameterCount()];
        callArgs[0] = title;
        callArgs[1] = mc;
        callArgs[2] = parsed;
        callArgs[3] = info;
        Class<?>[] types = connect.getParameterTypes();
        for (int i = 4; i < callArgs.length; i++) {
            callArgs[i] = types[i] == boolean.class ? Boolean.FALSE : null;
        }
        connect.invoke(null, callArgs);
    }

    // some modpacks ship wireguard-tunnel, which routes every connection
    // (localhost included) through Cloudflare WARP, so the local proxy is never
    // reached. the flag is flipped in memory only, without save(), so WARP comes
    // back after a restart. sorry Cloudflare
    private static String bypassTunnel(ClassLoader cl) {
        try {
            Class<?> cfg = Class.forName("codes.dreaming.wireguard.WireguardConfig", false, cl);
            Object instance = cfg.getMethod("getInstance").invoke(null);
            Field enabled = field(cfg, "warpEnabled");
            if (enabled.getBoolean(instance)) {
                enabled.setBoolean(instance, false);
                return "WARP (wireguard-tunnel) disabled until the game restarts";
            }
        } catch (ClassNotFoundException ignored) {
            // mod not installed
        } catch (Exception e) {
            return "wireguard-tunnel is installed but could not be disabled: " + e;
        }
        return null;
    }

    private static Field field(Class<?> c, String name) throws NoSuchFieldException {
        Field f = c.getDeclaredField(name);
        // private is more of a suggestion
        f.setAccessible(true);
        return f;
    }

    private static String describe(Throwable t) {
        while (t.getCause() != null && t instanceof InvocationTargetException) {
            t = t.getCause();
        }
        return "error: " + t;
    }

    private static void write(Path path, String text) {
        try {
            Files.writeString(path, text, StandardCharsets.UTF_8);
        } catch (Exception ignored) {
            // nothing else to report to
        }
    }
}
