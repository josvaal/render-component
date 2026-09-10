// C12 fixture: templateUrl points at a file that does not exist.
import { Component } from '@angular/core';

@Component({
  selector: 'app-ghost',
  standalone: true,
  templateUrl: './ghost.component.html',
})
export class GhostComponent {}
