package com.teddytennant.wizard.baselineprofile

import androidx.benchmark.macro.junit4.BaselineProfileRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import androidx.test.uiautomator.Until
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Startup, then the paths people take most: home, Settings and back,
 * Machines and back, the add-machine form. A chat needs a reachable
 * machine, so it isn't covered here.
 */
@RunWith(AndroidJUnit4::class)
class BaselineProfileGenerator {
    @get:Rule val rule = BaselineProfileRule()

    @Test
    fun generate() = rule.collect(packageName = "com.teddytennant.wizard", includeInStartupProfile = true) {
        pressHome()
        startActivityAndWait()
        device.wait(Until.findObject(By.text("Skip setup")), 3_000)?.click()
        device.wait(Until.hasObject(By.desc("Settings")), 5_000)
        repeat(2) {
            device.findObject(By.desc("Settings"))?.click()
            device.wait(Until.hasObject(By.text("Appearance")), 3_000)
            // The row, not the section label above it.
            device.findObject(By.clickable(true).hasDescendant(By.textStartsWith("Machines")))?.click()
            device.wait(Until.hasObject(By.desc("Add machine")), 3_000)
            device.findObject(By.desc("Add machine"))?.click()
            device.wait(Until.hasObject(By.text("Sign in with")), 3_000)
            device.pressBack()
            device.pressBack()
            device.pressBack()
            device.wait(Until.hasObject(By.desc("Message")), 3_000)
            device.findObject(By.desc("Message"))?.click()
            device.pressBack()
        }
    }
}
