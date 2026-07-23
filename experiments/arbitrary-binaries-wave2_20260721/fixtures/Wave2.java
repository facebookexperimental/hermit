public final class Wave2 {
    public static void main(String[] args) throws Exception {
        final long[] results = new long[4];
        Thread[] threads = new Thread[results.length];
        for (int i = 0; i < threads.length; i++) {
            final int index = i;
            threads[i] = new Thread(() -> {
                long total = 0;
                for (int n = 0; n < 100000; n++) {
                    total += n ^ index;
                }
                results[index] = total;
            });
            threads[i].start();
        }
        long total = 0;
        for (int i = 0; i < threads.length; i++) {
            threads[i].join();
            total += results[i];
        }
        System.out.println("java-ok " + total);
    }
}
