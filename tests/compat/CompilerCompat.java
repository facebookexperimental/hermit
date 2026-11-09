public final class CompilerCompat {
    public static void main(String[] args) {
        long previous = 0;
        long current = 1;
        for (int index = 0; index < 30; ++index) {
            long next = previous + current;
            previous = current;
            current = next;
        }
        if (previous != 832040) {
            throw new AssertionError(previous);
        }
        System.out.println("javac:" + previous);
    }
}
