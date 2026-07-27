import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public final class Threaded {
  private static final int THREADS = 8;
  private static final int ITERATIONS = 10_000;

  public static void main(String[] args) throws Exception {
    CountDownLatch ready = new CountDownLatch(THREADS);
    CountDownLatch start = new CountDownLatch(1);
    CountDownLatch done = new CountDownLatch(THREADS);
    AtomicInteger counter = new AtomicInteger();
    List<Thread> workers = new ArrayList<>();

    for (int i = 0; i < THREADS; i++) {
      Thread worker = new Thread(() -> {
        ready.countDown();
        try {
          start.await();
          for (int j = 0; j < ITERATIONS; j++) {
            counter.incrementAndGet();
          }
        } catch (InterruptedException error) {
          Thread.currentThread().interrupt();
          throw new RuntimeException(error);
        } finally {
          done.countDown();
        }
      });
      workers.add(worker);
      worker.start();
    }

    ready.await();
    start.countDown();
    done.await();
    int expected = THREADS * ITERATIONS;
    if (counter.get() != expected) {
      throw new AssertionError("expected " + expected + ", got " + counter.get());
    }
    System.out.println("SHARED_FUTEX_JAVA_OK threads=" + THREADS + " count=" + counter.get());
  }
}
