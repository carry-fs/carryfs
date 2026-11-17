package com.carryfs.android

import android.hardware.usb.UsbManager
import android.os.Bundle
import android.provider.DocumentsContract
import androidx.test.platform.app.InstrumentationRegistry
import java.io.DataInputStream
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

/**
 * Copy throughput to and from a real USB drive through the documents
 * provider, the path the Files app uses. Writes and then deletes a 256 MiB
 * file. Runs only when asked for: `am instrument -e usb true -e class
 * com.carryfs.android.UsbBenchmark`, with a CarryFS drive plugged in and USB
 * access already granted.
 */
class UsbBenchmark {
    private companion object {
        const val NAME = "carryfs-benchmark.bin"
    }

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext
    private val resolver = context.contentResolver

    @Test
    fun copyToAndFromUsb() {
        assumeTrue(InstrumentationRegistry.getArguments().getString("usb") == "true")
        val usb = context.getSystemService(UsbManager::class.java)
        val device = usb.deviceList.values.single { Drives.massStorage(it) != null }
        assumeTrue("Grant USB access in the app first", usb.hasPermission(device))

        Drives.mount(context, device)
        try {
            val volume = Drives.volumes().single()
            val root = DocumentsContract.buildDocumentUri(CarryProvider.AUTHORITY, CarryProvider.documentId(volume, Native.ROOT))
            // A run that lost the drive mid-way leaves its file behind.
            Native.children(volume, Native.ROOT).filter { it.name == NAME }.forEach { Native.delete(volume, it.id) }
            val file = DocumentsContract.createDocument(resolver, root, "application/octet-stream", NAME)!!
            val chunk = ByteArray(128 shl 10) { (it * 31).toByte() }
            val total = 256L shl 20
            try {
                // Closing publishes the file, so the write time includes reaching the drive.
                val start = System.nanoTime()
                val write = seconds {
                    resolver.openOutputStream(file)!!.use { out ->
                        repeat((total / chunk.size).toInt()) { i ->
                            out.write(chunk)
                            val written = (i + 1L) * chunk.size
                            if (written % (64 shl 20) == 0L) {
                                report("%d MiB at %.1f s".format(written shr 20, (System.nanoTime() - start) / 1e9))
                            }
                        }
                    }
                }
                // Every chunk must come back exactly as written; checking is timed apart from reading.
                val back = ByteArray(chunk.size)
                var checking = 0.0
                val read = seconds {
                    DataInputStream(resolver.openInputStream(file)!!).use { input ->
                        repeat((total / chunk.size).toInt()) { i ->
                            input.readFully(back)
                            checking += seconds { assertTrue("chunk $i differs", back.contentEquals(chunk)) }
                        }
                        assertEquals(-1, input.read())
                    }
                } - checking
                report("write %.1f MB/s, read %.1f MB/s (256 MiB, bytes checked in %.2f s)".format(total / write / 1e6, total / read / 1e6, checking))
            } finally {
                DocumentsContract.deleteDocument(resolver, file)
            }
        } finally {
            Drives.eject(context, device.deviceId.toLong())
        }
    }

    private inline fun seconds(block: () -> Unit): Double {
        val start = System.nanoTime()
        block()
        return (System.nanoTime() - start) / 1e9
    }

    private fun report(line: String) =
        instrumentation.sendStatus(0, Bundle().apply { putString("stream", "UsbBenchmark: $line\n") })
}
