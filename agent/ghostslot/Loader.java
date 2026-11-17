package ghostslot;

import java.lang.instrument.Instrumentation;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;

// a JVM keeps the first version of an agent class it loaded (sentimental), so
// the real logic comes from a fresh class loader on every attach. that way a
// rebuilt agent takes effect without restarting the game.
// args: "jar|host:port|result-file"
public final class Loader {
    private Loader() {}

    public static void agentmain(String args, Instrumentation inst) throws Exception {
        String[] parts = args.split("\\|", 3);
        URL jar = Path.of(parts[0]).toUri().toURL();
        ClassLoader loader = new URLClassLoader(new URL[] {jar}, ClassLoader.getPlatformClassLoader());
        loader.loadClass("ghostslot.Agent")
                .getMethod("run", String.class, String.class, Instrumentation.class)
                .invoke(null, parts[1], parts[2], inst);
    }
}
