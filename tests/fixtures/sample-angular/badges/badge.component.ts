import { Component } from '@angular/core';

// NOTE: `standalone: false` is explicit — since Angular v19 components default
// to standalone, and a standalone component cannot be declared by an NgModule.
@Component({
  selector: 'app-badge',
  standalone: false,
  templateUrl: './badge.component.html',
  styleUrls: ['./badge.component.scss'],
})
export class BadgeComponent {}
