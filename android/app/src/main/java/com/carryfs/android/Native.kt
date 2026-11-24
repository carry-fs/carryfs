package com.carryfs.android

import java.io.IOException
import java.nio.ByteBuffer
import java.nio.ByteOrder

class NativeIoException(message: String, val errno: Int) : IOException(message)

/** A file or folder on a mounted volume. */
class Entry(
    val id: Long,
    val parent: Long,
    val name: String,
    val directory: Boolean,
    val size: Long,
    val modified: Long,
)

/**
 * The Rust adapter in android/native. Every call is serialized there and
 * throws FileNotFoundException for a missing object, IOException otherwise.
 */
object Native {
    init {
        System.loadLibrary("carryfs_android")
    }

    /** The object ID of every volume's root folder. */
    const val ROOT = 1L

    /** Mount the CarryFS volumes of a claimed USB mass-storage interface. */
    @JvmStatic external fun mountUsb(drive: Long, fd: Int, iface: Int, endpointIn: Int, endpointOut: Int): LongArray
    /** Mount the CarryFS volumes of a disk image, standing in for a USB drive. */
    @JvmStatic external fun mountImage(drive: Long, path: String): LongArray
    @JvmStatic external fun takeMountWarning(drive: Long): String
    /** Close every file, publish, and release the drive. */
    @JvmStatic external fun eject(drive: Long)
    /** Forget a drive that was unplugged. Unpublished changes are lost. */
    @JvmStatic external fun detach(drive: Long)

    @JvmStatic external fun label(volume: Long): String
    /** Available and total bytes. */
    @JvmStatic external fun space(volume: Long): LongArray
    @JvmStatic external fun readOnly(volume: Long): Boolean
    /** Whether CarryFS 0.1.0 formatted the volume, so it is read-only until upgraded. */
    @JvmStatic external fun needsUpgrade(volume: Long): Boolean
    /** Make a CarryFS 0.1.0 volume writable. CarryFS 0.1.0 can't mount it afterwards. */
    @JvmStatic external fun upgrade(volume: Long)

    @JvmStatic private external fun listPage(volume: Long, directory: Long, after: String): ByteArray
    @JvmStatic private external fun stat(volume: Long, id: Long): ByteArray
    @JvmStatic external fun create(volume: Long, parent: Long, name: String, directory: Boolean): Long
    @JvmStatic external fun rename(volume: Long, id: Long, name: String)
    @JvmStatic external fun move(volume: Long, id: Long, from: Long, to: Long)
    /** Delete a file or directory tree and return its parent. */
    @JvmStatic external fun delete(volume: Long, id: Long): Long

    @JvmStatic external fun open(volume: Long, id: Long, write: Boolean, truncate: Boolean): Long
    @JvmStatic external fun read(volume: Long, file: Long, offset: Long, size: Int, buffer: ByteArray): Int
    @JvmStatic external fun write(volume: Long, file: Long, offset: Long, size: Int, buffer: ByteArray)
    @JvmStatic external fun size(volume: Long, file: Long): Long
    @JvmStatic external fun sync(volume: Long)
    /** Close a file, publishing it if it was written. */
    @JvmStatic external fun close(volume: Long, file: Long)

    fun children(volume: Long, directory: Long): List<Entry> = buildList {
        visitChildren(volume, directory) { add(it) }
    }

    fun visitChildren(volume: Long, directory: Long, visit: (Entry) -> Unit) {
        var after = ""
        do {
            val page = ByteBuffer.wrap(listPage(volume, directory, after)).order(ByteOrder.LITTLE_ENDIAN)
            after = readName(page)
            while (page.hasRemaining()) visit(readEntry(page))
        } while (after.isNotEmpty())
    }

    fun entry(volume: Long, id: Long): Entry =
        readEntry(ByteBuffer.wrap(stat(volume, id)).order(ByteOrder.LITTLE_ENDIAN))

    /** The layout `encode` in android/native/src/bridge.rs writes. */
    private fun readEntry(buffer: ByteBuffer): Entry {
        val id = buffer.long
        val parent = buffer.long
        val directory = buffer.get() != 0.toByte()
        val size = buffer.long
        val modified = buffer.long
        return Entry(id, parent, readName(buffer), directory, size, modified)
    }

    private fun readName(buffer: ByteBuffer): String =
        ByteArray(buffer.int).also { buffer.get(it) }.toString(Charsets.UTF_8)
}
