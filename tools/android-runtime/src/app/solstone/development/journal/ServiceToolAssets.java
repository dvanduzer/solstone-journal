// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;

import android.content.Context;
import android.system.Os;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.Arrays;
import org.json.*;

// Startup authority comes from app metadata and signed assets, never owner IPC.
final class ServiceToolAssets {
    static final int MAX_ASSET_BYTES = 8 * 1024 * 1024;

    static byte[] readAsset(Context context, String name) throws IOException {
        try (InputStream input = context.getAssets().open(name);
             ByteArrayOutputStream output = new ByteArrayOutputStream()) {
            byte[] buffer = new byte[16384]; int size;
            while ((size = input.read(buffer)) != -1) {
                if (output.size() > MAX_ASSET_BYTES - size) throw new IOException("service asset exceeds bound");
                output.write(buffer, 0, size);
            }
            return output.toByteArray();
        }
    }

    static File directory(File parent, String name) throws Exception {
        File result = new File(parent, name);
        if (Files.isSymbolicLink(result.toPath())) throw new IOException("service resource directory is a symlink");
        if (!result.exists() && !result.mkdir()) throw new IOException("service resource directory creation failed");
        if (!result.isDirectory()) throw new IOException("service resource directory required");
        File canonical = result.getCanonicalFile();
        if (!canonical.getParentFile().equals(parent)) throw new IOException("service resource directory escaped app files");
        Os.chmod(canonical.toString(), 0700);
        return canonical;
    }

    static void materialize(File root, String name, byte[] bytes) throws Exception {
        if (!name.matches("[A-Za-z0-9][A-Za-z0-9_.-]*")) throw new IOException("unsafe service resource filename");
        Path file = new File(root, name).toPath();
        if (Files.isSymbolicLink(file)) throw new IOException("service resource member is a symlink");
        if (Files.exists(file)) {
            if (!Files.isRegularFile(file) || Files.size(file) != bytes.length
                    || !Arrays.equals(Files.readAllBytes(file), bytes)) {
                throw new IOException("existing service resource differs from signed asset: " + name);
            }
            Os.chmod(file.toString(), 0600);
            return;
        }
        Path stage = Files.createTempFile(root.toPath(), ".service-tool-", ".stage");
        try {
            Os.chmod(stage.toString(), 0600);
            try (FileOutputStream output = new FileOutputStream(stage.toFile())) {
                output.write(bytes); output.getFD().sync();
            }
            Files.move(stage, file, StandardCopyOption.ATOMIC_MOVE);
        } finally { Files.deleteIfExists(stage); }
    }

    static String install(Context context) throws Exception {
        String[] entries = context.getAssets().list("service-tools");
        if (entries == null || entries.length == 0) return "{\"ok\":true,\"packaged\":false}";
        byte[] manifest = readAsset(context, "service-tools/manifest.json");
        byte[] receipt = readAsset(context, "service-tools/mobile-native-build.json");
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        digest.update(manifest); digest.update((byte)0); digest.update(receipt);
        StringBuilder identity = new StringBuilder();
        for (byte value: digest.digest()) identity.append(String.format(java.util.Locale.ROOT, "%02x", value & 255));
        File base = directory(context.getFilesDir().getCanonicalFile(), "service-tools");
        File resources = directory(base, identity.toString());
        JSONObject parsed = new JSONObject(new String(manifest, StandardCharsets.UTF_8));
        JSONArray files = parsed.getJSONArray("files");
        if (files.length() != 8) throw new IOException("complete service manifest required");
        for (int i = 0; i < files.length(); i++) {
            JSONObject member = files.getJSONObject(i);
            if (!member.getString("root").equals("resource")) continue;
            String name = member.getString("path");
            if (!name.matches("[A-Za-z0-9][A-Za-z0-9_.-]*")) throw new IOException("unsafe service resource filename");
            materialize(resources, name, readAsset(context, "service-tools/resources/" + name));
        }
        JSONObject result = new JSONObject(Native.admitServiceTools(
            new File(context.getApplicationInfo().nativeLibraryDir).getCanonicalPath(),
            resources.toString(), manifest, receipt));
        result.put("packaged", true);
        return result.toString();
    }
}
