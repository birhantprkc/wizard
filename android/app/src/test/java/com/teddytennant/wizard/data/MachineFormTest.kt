package com.teddytennant.wizard.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MachineFormTest {
    @Test
    fun validatesEachField() {
        val ok = MachineForm.validate("devbox", "devbox.local", "22", "teddy", AuthKind.Key, "k1", "", false)
        assertTrue(ok.ok)
        val bad = MachineForm.validate("", "-oProxyCommand=x", "70000", "", AuthKind.Key, null, "", false)
        assertNotNull(bad.name); assertNotNull(bad.host); assertNotNull(bad.port); assertNotNull(bad.user); assertNotNull(bad.auth)
        assertNotNull(MachineForm.validate("a", "h", "22", "u", AuthKind.Password, null, "", false).auth)
        assertNull(MachineForm.validate("a", "h", "22", "u", AuthKind.Password, null, "", true).auth)
        assertTrue(MachineForm.validate("a", "fe80::1%wlan0", "22", "u", AuthKind.Key, "k", "", false).ok)
    }

    @Test
    fun splitsPastedAddresses() {
        assertEquals(Triple("teddy", "devbox.local", 2222), MachineForm.splitAddress("teddy@devbox.local:2222"))
        assertEquals(Triple("pi", "192.168.1.40", null), MachineForm.splitAddress(" pi@192.168.1.40 "))
        assertEquals(Triple(null, "devbox", null), MachineForm.splitAddress("devbox"))
    }

    @Test
    fun recentDirsAreNewestFirstAndCapped() {
        var m = Machine("id", "n", "h", user = "u")
        (1..10).forEach { m = m.withRecentDir("/p$it") }
        m = m.withRecentDir("/p5")
        assertEquals("/p5", m.recentDirs.first())
        assertEquals(8, m.recentDirs.size)
        assertEquals(1, m.recentDirs.count { it == "/p5" })
        assertEquals("u@h", m.address)
        assertEquals("u@h:2222", m.copy(port = 2222).address)
    }
}
