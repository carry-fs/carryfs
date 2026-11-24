package com.carryfs.benchmark

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.util.Log
import android.view.WindowManager
import android.widget.TextView
import java.io.Closeable
import java.io.DataInputStream
import java.nio.ByteBuffer
import java.util.Random
import java.util.UUID
import kotlin.concurrent.thread
import kotlin.system.measureNanoTime

/** An independent SAF client, so both providers receive calls across Binder. */
class BenchmarkActivity : Activity() {
    private companion object {
        const val TAG = "CarryFsBench"
        const val PICK_FOLDER = 1
        const val CARRY_PROVIDER = "com.carryfs.android.documents"
        const val STORAGE_PROVIDER = "com.android.externalstorage.documents"
        const val MIB = 1 shl 20
        const val SEQUENTIAL_MIB = 512
        const val SMALL_FILES = 1000
        const val SMALL_BYTES = 4096
    }

    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        status = TextView(this).apply {
            textSize = 18f
            setPadding(32, 32, 32, 32)
        }
        setContentView(status)
        val filesystem = intent.getStringExtra("filesystem")
        val volume = intent.getStringExtra("volume")
        val rounds = intent.getIntExtra("rounds", 3)
        if (filesystem !in listOf("carryfs", "exfat", "fat32") || rounds !in 1..100 ||
            (filesystem != "carryfs" && volume?.matches(Regex("[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}")) != true)
        ) {
            report("Set filesystem to carryfs, exfat or fat32, rounds to 1–100, and volume to the USB filesystem UUID for exFAT/FAT32.")
            return
        }
        report("Choose the benchmark folder on $filesystem.")
        if (savedInstanceState == null) {
            @Suppress("DEPRECATION")
            startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).putExtra(Intent.EXTRA_LOCAL_ONLY, true), PICK_FOLDER)
        }
    }

    @Deprecated("Uses the platform picker without an AndroidX dependency")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_FOLDER) return
        val tree = data?.data
        if (resultCode != RESULT_OK || tree == null) {
            report("Folder selection cancelled.")
            return
        }
        if (!DocumentsContract.isTreeUri(tree)) {
            report("The picker did not return a folder grant.")
            return
        }
        val filesystem = checkNotNull(intent.getStringExtra("filesystem"))
        val volume = intent.getStringExtra("volume")
        val rounds = intent.getIntExtra("rounds", 3)
        val id = DocumentsContract.getTreeDocumentId(tree)
        val selectedTarget = if (filesystem == "carryfs") {
            tree.authority == CARRY_PROVIDER
        } else {
            tree.authority == STORAGE_PROVIDER && id.substringBefore(':').equals(volume, ignoreCase = true)
        }
        if (!selectedTarget) {
            report("Selected folder is on ${tree.authority} ($id). Select a folder on $filesystem.")
            return
        }
        val parent = DocumentsContract.buildDocumentUriUsingTree(tree, id)
        val runId = UUID.randomUUID().toString()
        thread(name = "usb-benchmark") {
            try {
                val rows = buildString {
                    appendLine("disk\tlabel\tround\tphase\tmetric\tvalue\tunit\tstatus\treason")
                    for (round in 1..rounds) {
                        report("$filesystem round $round/$rounds")
                        val folder = create(parent, Document.MIME_TYPE_DIR, "carryfs-bench-$runId-$round")
                        Closeable { delete(folder) }.use {
                            fun sample(metric: String, value: Double, unit: String) {
                                check(value.isFinite() && value > 0) { "Invalid $metric result: $value" }
                                appendLine("usb\t$filesystem\t$round\tsaf\t$metric\t$value\t$unit\t\t")
                            }
                            sequential(folder, ::sample)
                            smallFiles(folder, ::sample)
                        }
                    }
                }
                val result = "results-$filesystem-$runId.tsv"
                openFileOutput(result, MODE_PRIVATE).bufferedWriter().use { it.write(rows) }
                report("Complete: $result")
            } catch (error: Exception) {
                val message = "Benchmark failed on $filesystem: ${error.message}"
                Log.e(TAG, message, error)
                runOnUiThread { status.text = message }
            }
        }
    }

    private fun sequential(folder: Uri, sample: (String, Double, String) -> Unit) {
        val file = create(folder, "application/octet-stream", "seq.bin")
        val chunk = ByteArray(MIB).also { Random(0).nextBytes(it) }
        val marker = ByteBuffer.wrap(chunk)
        output(file).use { out ->
            val write = measureNanoTime {
                repeat(SEQUENTIAL_MIB) { index ->
                    marker.putInt(0, index)
                    out.write(chunk)
                }
                out.fd.sync()
            }
            sample("sequential_write_fsync", SEQUENTIAL_MIB * 1e9 / write, "MiB/s")
        }
        val back = ByteArray(MIB)
        var read = 0L
        DataInputStream(checkNotNull(contentResolver.openInputStream(file))).use { input ->
            repeat(SEQUENTIAL_MIB) { index ->
                read += measureNanoTime { input.readFully(back) }
                marker.putInt(0, index)
                check(back.contentEquals(chunk)) { "Readback differs at MiB $index" }
            }
            check(input.read() == -1) { "Unexpected data after $SEQUENTIAL_MIB MiB" }
        }
        sample("sequential_read_after_write", SEQUENTIAL_MIB * 1e9 / read, "MiB/s")
        delete(file)
    }

    private fun smallFiles(folder: Uri, sample: (String, Double, String) -> Unit) {
        val payload = ByteArray(SMALL_BYTES) { 'x'.code.toByte() }
        val names = List(SMALL_FILES) { "f$it.dat" }
        val files = ArrayList<Uri>(SMALL_FILES)
        val write = measureNanoTime {
            for (name in names) {
                val file = create(folder, "application/octet-stream", name)
                files.add(file)
                output(file).use { it.write(payload) }
            }
        }
        sample("small_file_create", SMALL_FILES * 1e9 / write, "files/s")

        val children = DocumentsContract.buildChildDocumentsUriUsingTree(folder, DocumentsContract.getDocumentId(folder))
        val projection = arrayOf(Document.COLUMN_DISPLAY_NAME, Document.COLUMN_SIZE)
        val listed = ArrayList<Pair<String, Long>>(SMALL_FILES)
        val list = measureNanoTime {
            checkNotNull(contentResolver.query(children, projection, null, null, null)).use { cursor ->
                while (cursor.moveToNext()) listed.add(cursor.getString(0) to cursor.getLong(1))
            }
        }
        check(listed.size == SMALL_FILES && listed.toSet() == names.map { it to SMALL_BYTES.toLong() }.toSet()) {
            "Small-file names or sizes differ"
        }
        sample("small_file_list", list / 1e6, "ms")
        val remove = measureNanoTime { files.forEach(::delete) }
        sample("small_file_remove", remove / 1e6, "ms")
    }

    private fun create(parent: Uri, mime: String, name: String): Uri =
        checkNotNull(DocumentsContract.createDocument(contentResolver, parent, mime, name)) { "Cannot create $name" }

    private fun delete(file: Uri) {
        check(DocumentsContract.deleteDocument(contentResolver, file)) { "Cannot delete $file" }
    }

    private fun output(file: Uri) =
        ParcelFileDescriptor.AutoCloseOutputStream(checkNotNull(contentResolver.openFileDescriptor(file, "w")))

    private fun report(message: String) {
        Log.i(TAG, message)
        runOnUiThread { status.text = message }
    }
}
