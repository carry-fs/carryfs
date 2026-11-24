package com.carryfs.android

import android.hardware.usb.UsbManager
import android.os.Bundle
import android.provider.DocumentsContract
import androidx.test.platform.app.InstrumentationRegistry
import com.carryfs.benchmark.DocumentWorkload
import java.io.Closeable
import java.io.File
import java.util.UUID
import org.junit.Assume.assumeTrue
import org.junit.Test

/** Runs the document workload with USB access already granted or a prepared debug image. */
class UsbBenchmark {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    @Test
    fun copyToAndFromUsb() {
        val arguments = InstrumentationRegistry.getArguments()
        val usbRun = arguments.getString("usb") == "true"
        val imageRun = arguments.getString("image") == "true"
        assumeTrue(usbRun || imageRun)
        require(!(usbRun && imageRun)) { "Choose USB or the benchmark image" }
        val rounds = arguments.getString("rounds", "1").toInt()
        require(rounds in 1..100) { "Rounds must be between 1 and 100" }

        val drive = if (usbRun) {
            val usb = context.getSystemService(UsbManager::class.java)
            val device = usb.deviceList.values.single { Drives.massStorage(it) != null }
            check(usb.hasPermission(device)) { "Grant USB access in the CarryFS app first" }
            Drives.mount(context, device)
            device.deviceId.toLong()
        } else {
            val image = File(context.filesDir, "benchmark.img")
            check(image.isFile) { "Put the benchmark image at files/benchmark.img" }
            Drives.mountImage(context, image)
            Drives.IMAGE
        }
        Closeable { Drives.eject(context, drive) }.use {
            val volume = Drives.volumes().single()
            val id = CarryProvider.documentId(volume, Native.ROOT)
            val tree = DocumentsContract.buildTreeDocumentUri(CarryProvider.AUTHORITY, id)
            val root = DocumentsContract.buildDocumentUriUsingTree(tree, id)
            val runId = UUID.randomUUID()
            val workload = DocumentWorkload(context.contentResolver)
            for (round in 1..rounds) {
                workload.run(root, "carryfs-bench-$runId-$round") { metric, value, unit ->
                    val line = "CarryFsBench: $round\t$metric\t$value\t$unit\n"
                    instrumentation.sendStatus(0, Bundle().apply { putString("stream", line) })
                }
            }
        }
    }
}
