// ProcessKit-family links and the current-project indicator for the sidebar.
//
// mdBook cannot use external URLs as SUMMARY chapters. The four reserved
// entries are draft prefix chapters instead; pinned mdBook v0.4.40 renders
// them as direct-child div elements which this script upgrades after load.
(function () {
  "use strict";

  var ENTRIES = {
    "Rust version": { href: "https://zelanton.github.io/ProcessKit-rs/" },
    "CLI runner": { placeholder: "Current project" },
    "Python wrapper": { href: "https://zelanton.github.io/processkit-py/" },
    ".NET version": { href: "https://zelanton.github.io/ProcessKit-fSharp/" }
  };

  function apply() {
    var drafts = document.querySelectorAll(
      ".sidebar .chapter li.chapter-item > div"
    );

    Array.prototype.forEach.call(drafts, function (draftEntry) {
      var textContent = draftEntry.textContent || "";
      var title = textContent.replace(/^\s*\d+\.\s*/, "").trim();
      var spec = ENTRIES[title];
      if (!spec) {
        return;
      }

      if (spec.href) {
        var link = document.createElement("a");
        link.href = spec.href;
        link.rel = "noopener";
        while (draftEntry.firstChild) {
          link.appendChild(draftEntry.firstChild);
        }
        draftEntry.replaceWith(link);
      } else {
        draftEntry.classList.add("current-implementation");
        draftEntry.title = spec.placeholder;
        draftEntry.setAttribute("aria-label", title + " — " + spec.placeholder);
      }
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", apply);
  } else {
    apply();
  }
})();
