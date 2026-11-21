package com.carryfs.android

import android.content.res.AssetFileDescriptor
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.ExifInterface
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import java.io.BufferedInputStream
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.io.InputStream
import java.util.UUID
import kotlin.math.max
import kotlin.math.roundToInt

/** Small upright JPEG previews of images on a volume, cached on the phone. */
object Thumbnails {
    private const val MAX_CACHE_BYTES = 32L * 1024 * 1024
    private const val READ_CHUNK = 64 * 1024
    private val cache = Any()
    private val namespace = UUID.randomUUID()

    /** Open a preview at most `size` pixels on its long side. Blocks on drive I/O. */
    fun open(cacheDir: File, volume: Long, entry: Entry, size: Int, signal: CancellationSignal?): AssetFileDescriptor {
        signal?.throwIfCanceled()
        val dir = File(cacheDir, "thumbnails").apply { mkdirs() }
        val version = Native.previewVersion(volume)
        val cached = File(dir, "$namespace-$volume-${entry.id}-$version-$size.jpg")
        synchronized(cache) {
            if (cached.isFile) return openCached(cached)
        }

        // Camera photos carry a small preview in their EXIF header, which costs only the first few kB to read.
        val exif = try {
            stream(volume, entry.id, signal).use { ExifInterface(it) }
        } catch (e: IOException) {
            null
        }
        signal?.throwIfCanceled()
        val embedded = exif?.thumbnail?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
        val bitmap = embedded?.takeIf { max(it.width, it.height) >= size / 2 }
            ?: decode(volume, entry.id, size, signal)
            ?: throw FileNotFoundException("${entry.name} has no readable image")
        val orientation = exif?.getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL)
            ?: ExifInterface.ORIENTATION_NORMAL

        signal?.throwIfCanceled()
        val temp = File.createTempFile("preview", ".tmp", dir)
        try {
            val success = temp.outputStream().use {
                upright(scaled(bitmap, size), orientation).compress(Bitmap.CompressFormat.JPEG, 85, it)
            }
            if (!success) throw IOException("Could not encode a preview")
            signal?.throwIfCanceled()
            if (Native.previewVersion(volume) != version) throw IOException("The volume changed during preview")
            return synchronized(cache) {
                if (!cached.isFile && !temp.renameTo(cached)) throw IOException("Could not store a preview")
                val preview = openCached(cached)
                trim(dir, cached)
                preview
            }
        } finally {
            temp.delete()
        }
    }

    private fun openCached(file: File): AssetFileDescriptor {
        val descriptor = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        file.setLastModified(System.currentTimeMillis())
        return AssetFileDescriptor(descriptor, 0, descriptor.statSize)
    }

    private fun trim(dir: File, keep: File) {
        val files = dir.listFiles { file -> file.extension == "jpg" } ?: return
        var remaining = files.sumOf { it.length() }
        for (file in files.sortedBy { it.lastModified() }) {
            if (remaining <= MAX_CACHE_BYTES) break
            if (file == keep) continue
            val bytes = file.length()
            if (file.delete()) remaining -= bytes
        }
    }

    /** Decode the whole image, sampled down to about `size`. */
    private fun decode(volume: Long, id: Long, size: Int, signal: CancellationSignal?): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        stream(volume, id, signal).use { BitmapFactory.decodeStream(it, null, bounds) }
        signal?.throwIfCanceled()
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        var sample = 1
        while (max(bounds.outWidth, bounds.outHeight) / (sample * 2) >= size) sample *= 2
        val options = BitmapFactory.Options().apply { inSampleSize = sample }
        return stream(volume, id, signal).use { BitmapFactory.decodeStream(it, null, options) }
    }

    private fun scaled(bitmap: Bitmap, size: Int): Bitmap {
        val scale = size.toFloat() / max(bitmap.width, bitmap.height)
        if (scale >= 1f) return bitmap
        val width = (bitmap.width * scale).roundToInt().coerceAtLeast(1)
        val height = (bitmap.height * scale).roundToInt().coerceAtLeast(1)
        return Bitmap.createScaledBitmap(bitmap, width, height, true)
    }

    /** Apply the EXIF rotation, since previews are served without it. Mirrored orientations are rare and left as is. */
    private fun upright(bitmap: Bitmap, orientation: Int): Bitmap {
        val degrees = when (orientation) {
            ExifInterface.ORIENTATION_ROTATE_90 -> 90f
            ExifInterface.ORIENTATION_ROTATE_180 -> 180f
            ExifInterface.ORIENTATION_ROTATE_270 -> 270f
            else -> return bitmap
        }
        val matrix = Matrix().apply { postRotate(degrees) }
        return Bitmap.createBitmap(bitmap, 0, 0, bitmap.width, bitmap.height, matrix, true)
    }

    private fun stream(volume: Long, id: Long, signal: CancellationSignal?): InputStream {
        signal?.throwIfCanceled()
        return BufferedInputStream(FileStream(volume, id, signal), READ_CHUNK)
    }

    /** A file on a volume read from the start. */
    private class FileStream(private val volume: Long, id: Long, private val signal: CancellationSignal?) : InputStream() {
        private val file = Native.open(volume, id, false, false)
        private var offset = 0L

        override fun read(): Int {
            val byte = ByteArray(1)
            return if (read(byte, 0, 1) == 1) byte[0].toInt() and 0xff else -1
        }

        override fun read(buffer: ByteArray, start: Int, length: Int): Int {
            signal?.throwIfCanceled()
            if (length == 0) return 0
            val count = length.coerceAtMost(READ_CHUNK)
            val target = if (start == 0) buffer else ByteArray(count)
            val read = Native.read(volume, file, offset, count, target)
            if (read <= 0) return -1
            if (start != 0) System.arraycopy(target, 0, buffer, start, read)
            offset += read
            return read
        }

        override fun skip(count: Long): Long {
            signal?.throwIfCanceled()
            val skipped = count.coerceIn(0, Native.size(volume, file) - offset)
            offset += skipped
            return skipped
        }

        override fun close() = Native.close(volume, file)
    }
}
