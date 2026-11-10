import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.FutureTask;

class Compat {
    public static void main(String[] args) throws Exception {
        // The work directory arrives as argv rather than being spelled out: the
        // heredoc is quoted, so $WORK_DIR cannot be interpolated here, and the
        // literal it used to hold silently stopped tracking the real directory
        // the moment that stopped being a fixed name.
        Path path = Paths.get(args[0], "data.txt");
        Files.write(path, Arrays.asList("gamma", "alpha", "beta"), StandardCharsets.UTF_8);
        List<String> lines = new ArrayList<>(Files.readAllLines(path));
        Collections.sort(lines);
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        byte[] hash = digest.digest(String.join(":", lines).getBytes(StandardCharsets.UTF_8));
        FutureTask<Integer> task = new FutureTask<>(() -> lines.stream().mapToInt(String::length).sum());
        Thread worker = new Thread(task, "compat-worker");
        worker.start();
        int total = task.get();
        StringBuilder prefix = new StringBuilder();
        for (int index = 0; index < 4; index++) {
            prefix.append(String.format("%02x", hash[index]));
        }
        System.out.printf("java:%d:%s:%b%n", total, prefix, System.currentTimeMillis() > 0);
    }
}
