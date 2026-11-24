package com.carryfs.benchmark

import android.content.ContentResolver
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import java.io.Closeable
import java.io.DataInputStream
import java.nio.ByteBuffer
import java.util.Random
import kotlin.system.measureNanoTime

internal class DocumentWorkload(private val contentResolver: ContentResolver) {
    private companion object {
        const val MIB = 1 shl 20
        const val SEQUENTIAL_MIB = 512
        // Larger requests can exhaust Android's AppFuse message buffer.
        const val IO_BYTES = 128 shl 10
        const val SMALL_FILES = 1000
        const val SMALL_BYTES = 4096
    }

    fun run(parent: Uri, name: String, smallOnly: Boolean = false, sample: (String, Double, String) -> Unit) {
        val folder = create(parent, Document.MIME_TYPE_DIR, name)
        Closeable { delete(folder) }.use {
            fun checkedSample(metric: String, value: Double, unit: String) {
                check(value.isFinite() && value > 0) { "Invalid $metric result: $value" }
                sample(metric, value, unit)
            }
            if (!smallOnly) sequential(folder, ::checkedSample)
            smallFiles(folder, ::checkedSample)
        }
    }

    private fun sequential(folder: Uri, sample: (String, Double, String) -> Unit) {
        val file = create(folder, "application/octet-stream", "seq.bin")
        val chunk = ByteArray(IO_BYTES).also { Random(0).nextBytes(it) }
        val chunks = SEQUENTIAL_MIB * MIB / chunk.size
        val marker = ByteBuffer.wrap(chunk)
        output(file).use { out ->
            val write = measureNanoTime {
                repeat(chunks) { index ->
                    marker.putInt(0, index)
                    out.write(chunk)
                }
                out.fd.sync()
            }
            sample("sequential_write_fsync", SEQUENTIAL_MIB * 1e9 / write, "MiB/s")
        }
        val back = ByteArray(chunk.size)
        var read = 0L
        DataInputStream(checkNotNull(contentResolver.openInputStream(file))).use { input ->
            repeat(chunks) { index ->
                read += measureNanoTime { input.readFully(back) }
                marker.putInt(0, index)
                check(back.contentEquals(chunk)) { "Readback differs at byte ${index * chunk.size}" }
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
}
