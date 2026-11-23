package com.carryfs.android

import android.app.Activity
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.ActivityNotFoundException
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.content.res.ColorStateList
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.provider.DocumentsContract
import android.text.TextUtils
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.LinearLayout.LayoutParams
import android.widget.ScrollView
import android.widget.TextView
import java.io.File
import kotlin.concurrent.thread
import kotlin.math.roundToInt

/**
 * Lists USB drives, mounts and ejects them, and opens their volumes in Files.
 *
 * Design rules: color only shows state, as one dot (orange in use, yellow
 * read-only, green safe to unplug, red error). Monospace is only for labels
 * and measurements. The volume name and used space are the only large type.
 * No cards or shadows; hairlines separate drives.
 */
class MainActivity : Activity() {
    companion object {
        private const val PERMISSION = "com.carryfs.android.USB_PERMISSION"
        private const val MATCH = LayoutParams.MATCH_PARENT
        private const val WRAP = LayoutParams.WRAP_CONTENT
    }

    /** A line of feedback; `dot` is a state color, or 0 for none. */
    private class Status(val text: String, val dot: Int = 0)

    private lateinit var list: LinearLayout
    private var status: Status? = null
    private var busy = false
    /** Volumes whose capacity bar has already played its fill. */
    private val filled = mutableSetOf<Long>()
    /** Volume labels of USB drives seen mounted, by device number and name. */
    private val labels = mutableMapOf<String, String>()

    private val usb by lazy { getSystemService(UsbManager::class.java) }
    private val testImage by lazy { File(filesDir, "test.img") }

    private val light = Typeface.create("sans-serif-light", Typeface.NORMAL)
    private val medium = Typeface.create("sans-serif-medium", Typeface.NORMAL)
    private val extraLight =
        if (Build.VERSION.SDK_INT >= 28) Typeface.create(Typeface.DEFAULT, 200, false) else light

    private val permission = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val device = intent.usbDevice() ?: return
            if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                mount(device)
            } else {
                status = Status("USB access to ${device.displayName()} was denied.", R.color.error)
                render()
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(24.dp, 0, 24.dp, 40.dp)
        }
        // Apps targeting API 35 draw edge to edge; keep content clear of the system bars.
        setContentView(ScrollView(this).apply {
            fitsSystemWindows = true
            addView(list)
        })

        val filter = IntentFilter(PERMISSION)
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(permission, filter, RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(permission, filter)
        }
        attached(intent)
    }

    override fun onDestroy() {
        unregisterReceiver(permission)
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        attached(intent)
    }

    override fun onResume() {
        super.onResume()
        Drives.onChange = ::render
        render()
        for (drive in Drives.all()) {
            thread {
                drive.refresh()
                runOnUiThread { if (!isDestroyed) render() }
            }
        }
    }

    override fun onPause() {
        Drives.onChange = null
        super.onPause()
    }

    /** A drive plugged in with this app chosen to handle it comes with access granted. */
    private fun attached(intent: Intent) {
        if (intent.action != UsbManager.ACTION_USB_DEVICE_ATTACHED) return
        intent.usbDevice()?.let(::mount)
    }

    private fun render() {
        list.removeAllViews()
        list.addView(header())
        status?.let { list.addView(statusLine(it), margins(top = 8)) }
        val drives = Drives.all()
        val keys = drives.map { it.key }.toSet()
        val mounted = drives.associateBy { it.key }
        val devices = usb.deviceList.values.filter { Drives.massStorage(it) != null }.sortedBy { it.deviceId }
        // Each drive keeps its place whether or not it is mounted; the disk image comes last.
        val sections: List<() -> Unit> =
            devices.map { device -> { mounted[device.deviceId.toLong()]?.let(::drive) ?: plugged(device) } } +
                drives.filter { drive -> devices.none { it.deviceId.toLong() == drive.key } }.map { { drive(it) } }
        sections.forEachIndexed { index, section ->
            if (index > 0) list.addView(hairline())
            section()
        }
        if (sections.isEmpty()) empty()
        if (drives.isNotEmpty()) {
            list.addView(
                text("Mounted volumes also appear in the Files app and in every app's file picker.", 13f, R.color.muted),
                margins(top = 40),
            )
        }
        if (devices.isNotEmpty()) {
            // Android can't read CarryFS, so its own storage notification offers to format the drive.
            list.addView(
                text("Android may also show \"Issue with USB drive\". Swipe it away. Its fix formats the drive and erases it.", 13f, R.color.muted),
                margins(top = if (drives.isNotEmpty()) 12 else 40),
            )
        }
        if (BuildConfig.DEBUG && Drives.IMAGE !in keys && testImage.exists()) list.addView(debugImage(), margins(top = 24))
    }

    private fun header() = LinearLayout(this).apply {
        gravity = Gravity.CENTER_VERTICAL
        minimumHeight = 56.dp
        addView(ImageView(context).apply {
            setImageResource(R.drawable.ic_cube)
            imageTintList = ColorStateList.valueOf(getColor(R.color.ink))
            contentDescription = null
        }, LayoutParams(20.dp, 20.dp))
        addView(text("CarryFS", 16f, R.color.ink, medium), LayoutParams(WRAP, WRAP).apply { marginStart = 10.dp })
        addView(View(context), LayoutParams(0, 0, 1f))
        addView(mono("v${BuildConfig.VERSION_NAME}"))
    }

    private fun drive(drive: Drives.Drive) {
        val single = drive.volumes.size == 1
        drive.volumes.forEachIndexed { index, volume ->
            volume(drive, volume, if (single) null else index + 1)
        }
        if (!single) list.addView(row(eject(drive)), margins(top = 24))
        if (drive.mountWarning.isNotEmpty()) {
            list.addView(statusLine(Status(drive.mountWarning, R.color.error)), margins(top = 16))
        }
    }

    private fun volume(drive: Drives.Drive, volume: Long, number: Int?) {
        val snapshot = drive.status[volume] ?: return
        val (name, space, legacy, readOnly) = snapshot

        if (drive.usb && number == null) labels["${drive.key}/${drive.name}"] = name
        list.addView(heading(name, 40f), margins(top = 24))
        val source = if (drive.usb) null else "DISK IMAGE"
        val meta = listOfNotNull(drive.name, source, number?.let { "VOLUME $it" }).joinToString(" · ")
        list.addView(mono(meta.uppercase()), margins(top = 4))
        val (dot, state) = when {
            legacy -> R.color.read_only to "Read-only. CarryFS 0.1.0 formatted it"
            readOnly -> R.color.read_only to "Read-only"
            else -> R.color.in_use to "In use. Eject before unplugging"
        }
        list.addView(statusLine(Status(state, dot)), margins(top = 16))

        if (space != null) {
            val (available, total) = space[0] to space[1]
            val used = (total - available).coerceAtLeast(0)
            list.addView(usage(used, total), margins(top = 28))
            val bar = CapacityBar(this, used, total).apply {
                contentDescription = "${size(used).joined()} used of ${size(total).joined()}"
            }
            list.addView(bar, margins(top = 16))
            if (filled.add(volume)) bar.post { bar.fill() }
            list.addView(LinearLayout(this).apply {
                addView(mono("USED"), LayoutParams(0, WRAP, 1f))
                addView(mono("${size(available).joined()} FREE".uppercase()))
            }, margins(top = 8))
        }

        list.addView(LinearLayout(this).apply {
            addView(fact("ACCESS", if (readOnly) "read only" else "read / write"), LayoutParams(0, WRAP, 1f))
            addView(fact("FORMAT", if (legacy) "revision 1" else "revision 2"), LayoutParams(0, WRAP, 1f))
        }, margins(top = 28))

        val open = pill("Open in Files", filled = true) { openFiles(volume) }
        list.addView(if (number == null) row(open, eject(drive)) else row(open), margins(top = 32))
        if (legacy) {
            list.addView(row(pill("Upgrade to enable writing", outlined = true) { confirmUpgrade(volume, name) }), margins(top = 10))
        }
    }

    /** A mass-storage device the app hasn't mounted yet. */
    private fun plugged(device: UsbDevice) {
        val name = device.displayName()
        val label = labels["${device.deviceId}/$name"]
        list.addView(heading(label ?: name, if (label != null) 40f else 28f), margins(top = 24))
        list.addView(mono(if (label != null) name.uppercase() else "USB MASS STORAGE"), margins(top = 4))
        list.addView(statusLine(Status("Not mounted. Safe to unplug", R.color.safe)), margins(top = 16))
        list.addView(row(pill("Mount", filled = true) { connect(device) }), margins(top = 28))
    }

    private fun empty() {
        list.addView(ImageView(this).apply {
            setImageResource(R.drawable.ic_cube)
            imageTintList = ColorStateList.valueOf(getColor(R.color.hairline))
            contentDescription = null
        }, LayoutParams(72.dp, 72.dp).apply { topMargin = 72.dp })
        list.addView(text("No drive", 40f, R.color.ink, light), margins(top = 24))
        list.addView(
            text("Connect a USB drive with a CarryFS partition through the USB-C port or an OTG adapter.", 15f, R.color.muted),
            margins(top = 8),
        )
        listOf(
            "Plug in the drive",
            "Allow USB access when Android asks",
            "Open the volume in Files",
        ).forEachIndexed { index, step ->
            list.addView(LinearLayout(this).apply {
                addView(mono("${index + 1}"), LayoutParams(28.dp, WRAP))
                addView(text(step, 15f, R.color.ink))
            }, margins(top = if (index == 0) 32 else 14))
        }
    }

    private fun debugImage() = LinearLayout(this).apply {
        gravity = Gravity.CENTER_VERTICAL
        minimumHeight = 48.dp
        addView(text("Mount test image", 14f, R.color.ink, medium).apply {
            paintFlags = paintFlags or Paint.UNDERLINE_TEXT_FLAG
        })
        addView(View(context), LayoutParams(0, 0, 1f))
        addView(mono("DEBUG"))
        isEnabled = !busy
        alpha = if (busy) 0.38f else 1f
        setOnClickListener {
            perform("Mounting the test image…", "Couldn't mount the test image", "Check files/test.img.") {
                Drives.mountImage(this@MainActivity, testImage)
                null
            }
        }
    }

    private fun usage(used: Long, total: Long) = LinearLayout(this).apply {
        val (number, unit) = size(used)
        val (totalNumber, totalUnit) = size(total)
        addView(text(number, 64f, R.color.ink, extraLight).apply {
            includeFontPadding = false
            fontFeatureSettings = "tnum"
        })
        val rest = if (unit == totalUnit) " / $totalNumber $totalUnit" else " $unit / $totalNumber $totalUnit"
        addView(text(rest, 24f, R.color.muted, light).apply { fontFeatureSettings = "tnum" })
    }

    private fun fact(key: String, value: String) = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        addView(mono(key))
        addView(text(value, 16f, R.color.ink), margins(top = 4))
    }

    private fun eject(drive: Drives.Drive) = pill("Eject", filled = false) {
        perform("Ejecting ${drive.name}…", "Couldn't eject ${drive.name}", "Unplug it only after ejecting.") {
            Drives.eject(this, drive.key)
            Status("${drive.name} ejected.")
        }
    }

    private fun confirmUpgrade(volume: Long, name: String) {
        AlertDialog.Builder(this)
            .setTitle("Upgrade $name?")
            .setMessage("CarryFS 0.1.0 formatted this volume, so it is read-only. After the upgrade, it takes writes, but computers with CarryFS 0.1.0 can no longer mount it. Update CarryFS on them first.")
            .setPositiveButton("Upgrade") { _, _ ->
                perform("Upgrading $name…", "Couldn't upgrade $name", "It stays read-only.") {
                    Drives.upgrade(this, volume)
                    Status("$name upgraded. It takes writes now.")
                }
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun connect(device: UsbDevice) {
        if (usb.hasPermission(device)) return mount(device)
        // Mutable, because the system adds the device and the grant to the intent.
        val flags = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
        val intent = PendingIntent.getBroadcast(this, 0, Intent(PERMISSION).setPackage(packageName), flags)
        usb.requestPermission(device, intent)
    }

    private fun mount(device: UsbDevice) {
        // The notification that keeps a USB drive mounted needs this, so ask when it first matters.
        val notifications = android.Manifest.permission.POST_NOTIFICATIONS
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(notifications) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(notifications), 0)
        }
        perform("Mounting ${device.displayName()}…", "Couldn't mount ${device.displayName()}", "Unplug the drive, plug it back in, and try again.") {
            Drives.mount(this, device)
            null
        }
    }

    private fun openFiles(volume: Long) {
        val intent = Intent(Intent.ACTION_VIEW)
            .setDataAndType(CarryProvider.rootUri(volume), DocumentsContract.Root.MIME_TYPE_ITEM)
        try {
            startActivity(intent)
        } catch (e: ActivityNotFoundException) {
            status = Status("No Files app can open the volume. Use any app's file picker instead.", R.color.error)
            render()
        }
    }

    /** Run blocking drive I/O off the main thread, showing progress and then how it ended. */
    private fun perform(progress: String, failed: String, recovery: String, work: () -> Status?) {
        busy = true
        status = Status(progress, R.color.muted)
        render()
        thread {
            val result = try {
                work()
            } catch (e: Exception) {
                Status("$failed: ${(e.message ?: e.toString()).trimEnd('.')}. $recovery", R.color.error)
            }
            runOnUiThread {
                busy = false
                status = result
                render()
            }
        }
    }

    private fun statusLine(status: Status) = LinearLayout(this).apply {
        val line = text(status.text, 14f, R.color.ink)
        if (status.dot != 0) {
            addView(View(context).apply {
                background = GradientDrawable().apply {
                    shape = GradientDrawable.OVAL
                    setColor(getColor(status.dot))
                }
            }, LayoutParams(10.dp, 10.dp).apply {
                marginEnd = 10.dp
                // Centred on the first line, so a wrapped message keeps its dot at the top.
                topMargin = (line.lineHeight - 10.dp) / 2
            })
        }
        addView(line, LayoutParams(0, WRAP, 1f))
    }

    private fun heading(value: String, size: Float) = text(value, size, R.color.ink, light).apply {
        maxLines = 2
        ellipsize = TextUtils.TruncateAt.END
        if (Build.VERSION.SDK_INT >= 28) isAccessibilityHeading = true
    }

    private fun row(vararg pills: View) = LinearLayout(this).apply {
        pills.forEachIndexed { index, pill ->
            addView(pill, LayoutParams(0, 52.dp, 1f).apply { if (index > 0) marginStart = 10.dp })
        }
    }

    private fun pill(label: String, filled: Boolean = false, outlined: Boolean = false, onClick: () -> Unit) =
        Button(this).apply {
            text = label
            isAllCaps = false
            textSize = 15f
            typeface = medium
            stateListAnimator = null
            minimumHeight = 0
            minHeight = 0
            setTextColor(getColor(if (filled) R.color.on_ink else R.color.ink))
            val shape = GradientDrawable().apply {
                cornerRadius = 26.dp.toFloat()
                setColor(getColor(if (filled) R.color.ink else if (outlined) R.color.ground else R.color.tonal))
                if (outlined) setStroke(1.dp, getColor(R.color.ink))
            }
            background = RippleDrawable(ColorStateList.valueOf(getColor(R.color.muted)), shape, null)
            isEnabled = !busy
            alpha = if (busy) 0.38f else 1f
            setOnClickListener { onClick() }
        }

    private fun hairline() = View(this).apply {
        setBackgroundColor(getColor(R.color.hairline))
        layoutParams = LayoutParams(MATCH, 1.dp).apply { topMargin = 40.dp }
    }

    private fun text(value: String, size: Float, color: Int, face: Typeface = Typeface.DEFAULT) = TextView(this).apply {
        text = value
        textSize = size
        typeface = face
        setTextColor(getColor(color))
    }

    /** Small monospace labels and measurements. */
    private fun mono(value: String) = text(value, 11f, R.color.muted, Typeface.MONOSPACE).apply {
        letterSpacing = 0.04f
        fontFeatureSettings = "tnum"
    }

    private fun margins(top: Int) = LayoutParams(MATCH, WRAP).apply { topMargin = top.dp }

    private val Int.dp get() = (this * resources.displayMetrics.density).roundToInt()

    /** Decimal units, as Android's own storage screens use. */
    private fun size(bytes: Long): Pair<String, String> {
        val units = listOf("B", "kB", "MB", "GB", "TB")
        var value = bytes.toDouble()
        var unit = 0
        while (value >= 1000 && unit < units.lastIndex) {
            value /= 1000
            unit++
        }
        val number = if (unit == 0 || value >= 100) "%.0f".format(value) else "%.1f".format(value)
        return number to units[unit]
    }

    private fun Pair<String, String>.joined() = "$first $second"
}
