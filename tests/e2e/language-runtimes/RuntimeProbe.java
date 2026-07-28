/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.time.Instant;
import java.time.ZoneOffset;
import java.time.ZonedDateTime;
import java.util.ArrayList;
import java.util.List;
import java.util.Random;
import java.util.TimeZone;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.ReentrantLock;

public final class RuntimeProbe {
  private static final int THREADS = 4;
  private static final int ITERATIONS = 64;

  private RuntimeProbe() {}

  private static String hex(byte[] bytes) {
    StringBuilder output = new StringBuilder();
    for (byte value : bytes) {
      output.append(String.format("%02x", value & 0xff));
    }
    return output.toString();
  }

  private static void randomProbe() {
    byte[] token = new byte[16];
    new SecureRandom().nextBytes(token);
    long value = new Random().nextLong();
    int hash = "hermit-runtime".hashCode();
    System.out.println("RANDOM token=" + hex(token) + " prng=" + value + " hash=" + hash);
  }

  private static void timeProbe() {
    TimeZone.setDefault(TimeZone.getTimeZone("UTC"));
    Instant now = Instant.now();
    ZonedDateTime utc = ZonedDateTime.ofInstant(now, ZoneOffset.UTC);
    System.out.println("TIME unix_ns=" + now.getEpochSecond() + String.format("%09d", now.getNano())
        + " utc=" + utc + " zone=" + utc.getOffset());
  }

  private static void threadProbe() throws Exception {
    ReentrantLock lock = new ReentrantLock();
    CountDownLatch ready = new CountDownLatch(THREADS);
    CountDownLatch start = new CountDownLatch(1);
    CountDownLatch done = new CountDownLatch(THREADS);
    AtomicInteger counter = new AtomicInteger();
    AtomicInteger nextRecord = new AtomicInteger();
    byte[] schedule = new byte[THREADS * ITERATIONS];
    List<Thread> workers = new ArrayList<>();

    for (int workerId = 0; workerId < THREADS; workerId++) {
      final int id = workerId;
      Thread worker = new Thread(() -> {
        ready.countDown();
        try {
          start.await();
          for (int iteration = 0; iteration < ITERATIONS; iteration++) {
            lock.lock();
            try {
              counter.incrementAndGet();
              schedule[nextRecord.getAndIncrement()] = (byte) id;
            } finally {
              lock.unlock();
            }
            Thread.yield();
          }
        } catch (InterruptedException error) {
          Thread.currentThread().interrupt();
          throw new RuntimeException(error);
        } finally {
          done.countDown();
        }
      }, "hermit-runtime-" + id);
      workers.add(worker);
      worker.start();
    }

    ready.await();
    start.countDown();
    done.await();
    for (Thread worker : workers) {
      worker.join();
    }

    int expected = THREADS * ITERATIONS;
    if (counter.get() != expected || nextRecord.get() != expected) {
      throw new AssertionError("thread counter mismatch: " + counter.get() + " != " + expected);
    }
    String digest = hex(MessageDigest.getInstance("SHA-256").digest(schedule));
    System.out.println("THREAD workers=" + THREADS + " counter=" + counter.get()
        + " schedule_sha256=" + digest);
  }

  private static void systemProbe() throws Exception {
    String sentinel = System.getenv("HERMIT_RUNTIME_SENTINEL");
    String name = "";
    String threads = "";
    for (String line : Files.readAllLines(Paths.get("/proc/self/status"), StandardCharsets.UTF_8)) {
      if (line.startsWith("Name:")) {
        name = line.substring(line.indexOf(':') + 1).trim();
      } else if (line.startsWith("Threads:")) {
        threads = line.substring(line.indexOf(':') + 1).trim();
      }
    }
    String procHostname = new String(
        Files.readAllBytes(Paths.get("/proc/sys/kernel/hostname")), StandardCharsets.UTF_8).trim();
    System.out.println("SYSTEM uname=" + System.getProperty("os.name") + "/"
        + System.getProperty("os.arch") + " env=" + sentinel + " proc=" + name + "/"
        + threads + "/" + procHostname);
  }

  public static void main(String[] args) throws Exception {
    randomProbe();
    timeProbe();
    threadProbe();
    systemProbe();
  }
}
