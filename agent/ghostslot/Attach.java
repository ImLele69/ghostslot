package ghostslot;

import com.sun.tools.attach.VirtualMachine;

// loads an agent jar into a running JVM through the standard Attach API, needs a JDK
public final class Attach {
    private Attach() {}

    public static void main(String[] args) throws Exception {
        if (args.length != 3) {
            System.err.println("usage: Attach <pid> <agent.jar> <options>");
            System.exit(2);
        }
        VirtualMachine vm = VirtualMachine.attach(args[0]);
        try {
            vm.loadAgent(args[1], args[2]);
        } finally {
            vm.detach();
        }
    }
}
