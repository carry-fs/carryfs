package com.carryfs.android

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.ExifInterface
import java.io.BufferedInputStream
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.io.InputStream
import kotlin.math.max
import kotlin.math.roundToInt

/** Small upright JPEG previews of images on a volume, cached on the phone. */
object Thumbnails {
    /** A preview of `entry` at most `size` pixels on its long side. Blocks on drive I/O. */
    fun file(cacheDir: File, volume: Long, entry: Entry, size: Int): File {
        val dir = File(cacheDir, "thumbnails").apply { mkdirs() }
        // The size and modification time change whenever the file does, so a stale preview is never served.
        // ponytail: never evicted; Android clears app caches when storage runs low.
        val cached = File(dir, "$volume-${entry.id}-${entry.size}-${entry.modified}-$size.jpg")
        if (cached.exists()) return cached

        // Camera photos carry a small preview in their EXIF header, which costs only the first few kB to read.
        val exif = try {
            stream(volume, entry.id).use { ExifInterface(it) }
        } catch (e: IOException) {
            null
        }
        val embedded = exif?.thumbnail?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
        val bitmap = embedded?.takeIf { max(it.width, it.height) >= size / 2 }
            ?: decode(volume, entry.id, size)
            ?: throw FileNotFoundException("${entry.name} has no readable image")
        val orientation = exif?.getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL)
            ?: ExifInterface.ORIENTATION_NORMAL

        val temp = File.createTempFile("preview", ".tmp", dir)
        temp.outputStream().use { upright(scaled(bitmap, size), orientation).compress(Bitmap.CompressFormat.JPEG, 85, it) }
        if (!temp.renameTo(cached)) temp.delete()
        return cached
    }

    /** Decode the whole image, sampled down to about `size`. */
    private fun decode(volume: Long, id: Long, size: Int): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        stream(volume, id).use { BitmapFactory.decodeStream(it, null, bounds) }
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        var sample = 1
        while (max(bounds.outWidth, bounds.outHeight) / (sample * 2) >= size) sample *= 2
        val options = BitmapFactory.Options().apply { inSampleSize = sample }
        return stream(volume, id).use { BitmapFactory.decodeStream(it, null, options) }
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

    private fun stream(volume: Long, id: Long): InputStream = BufferedInputStream(FileStream(volume, id), 256 shl 10)

    /** A file on a volume read from the start. */
    private class FileStream(private val volume: Long, id: Long) : InputStream() {
        private val file = Native.open(volume, id, false, false)
        private var offset = 0L

        override fun read(): Int {
            val byte = ByteArray(1)
            return if (read(byte, 0, 1) == 1) byte[0].toInt() and 0xff else -1
        }

        override fun read(buffer: ByteArray, start: Int, length: Int): Int {
            if (length == 0) return 0
            val target = if (start == 0) buffer else ByteArray(length)
            val read = Native.read(volume, file, offset, length, target)
            if (read <= 0) return -1
            if (start != 0) System.arraycopy(target, 0, buffer, start, read)
            offset += read
            return read
        }

        override fun skip(count: Long): Long {
            val skipped = count.coerceIn(0, Native.size(volume, file) - offset)
            offset += skipped
            return skipped
        }

        override fun close() = Native.close(volume, file)
    }
}
